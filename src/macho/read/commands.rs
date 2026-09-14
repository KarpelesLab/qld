//! Load commands: iteration and typed decoders.
//!
//! [`LoadCommandIter`] yields each command's bytes after checking its size.
//! The `LoadCommand::*` decoders then interpret one command; each checks that
//! the command is large enough for its fixed fields and that any string it
//! names lies inside it.

use super::bytes::{Endian, Source, cstr, fixed_name, read_uleb, to_u64};
use super::consts::{
    LC_BUILD_VERSION, LC_ID_DYLIB, LC_LAZY_LOAD_DYLIB, LC_LOAD_DYLIB, LC_LOAD_UPWARD_DYLIB,
    LC_LOAD_WEAK_DYLIB, LC_REEXPORT_DYLIB, LC_SEGMENT, LC_SEGMENT_64, LC_VERSION_MIN_IPHONEOS,
    LC_VERSION_MIN_MACOSX, LC_VERSION_MIN_TVOS, LC_VERSION_MIN_WATCHOS, PLATFORM_IOS,
    PLATFORM_MACOS, PLATFORM_TVOS, PLATFORM_WATCHOS, load_command_name,
};
use super::section::{SECTION_64_SIZE, SECTION_SIZE, SectionTable};
use crate::error::Result;

/// One load command: its type and its bytes (including the 8-byte
/// `cmd`/`cmdsize` prefix).
#[derive(Clone, Copy, Debug)]
pub struct LoadCommand<'a> {
    /// `cmd`.
    pub cmd: u32,
    /// File offset of the command.
    pub offset: u64,
    /// The whole command, `cmdsize` bytes.
    pub data: &'a [u8],
    endian: Endian,
    is64: bool,
    source: Source<'a>,
}

/// Iterator over load commands.
///
/// Stops after `ncmds` commands. A command whose `cmdsize` is smaller than 8
/// or runs past `sizeofcmds` is reported as an error and ends iteration.
#[derive(Clone, Debug)]
pub struct LoadCommandIter<'a> {
    data: &'a [u8],
    pos: usize,
    base: u64,
    remaining: u32,
    endian: Endian,
    is64: bool,
    source: Source<'a>,
}

impl<'a> LoadCommandIter<'a> {
    pub(crate) fn new(
        data: &'a [u8],
        base: u64,
        ncmds: u32,
        endian: Endian,
        is64: bool,
        source: Source<'a>,
    ) -> Self {
        Self {
            data,
            pos: 0,
            base,
            remaining: ncmds,
            endian,
            is64,
            source,
        }
    }
}

impl<'a> Iterator for LoadCommandIter<'a> {
    type Item = Result<LoadCommand<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining = self.remaining.saturating_sub(1);
        let offset = self.base.saturating_add(to_u64(self.pos));
        let fail = |this: &mut Self, what: &str| {
            this.remaining = 0;
            Some(Err(this.source.malformed(offset, what.to_owned())))
        };
        let (Some(cmd), Some(cmdsize)) = (
            self.endian.u32(self.data, self.pos),
            self.pos
                .checked_add(4)
                .and_then(|at| self.endian.u32(self.data, at)),
        ) else {
            return fail(self, "load command (extends past sizeofcmds)");
        };
        let Ok(size) = usize::try_from(cmdsize) else {
            return fail(self, "load command size");
        };
        if size < 8 {
            return fail(self, "load command size (smaller than 8)");
        }
        let Some(data) = self
            .pos
            .checked_add(size)
            .and_then(|end| self.data.get(self.pos..end))
        else {
            return fail(self, "load command (extends past sizeofcmds)");
        };
        self.pos = self.pos.saturating_add(size);
        Some(Ok(LoadCommand {
            cmd,
            offset,
            data,
            endian: self.endian,
            is64: self.is64,
            source: self.source,
        }))
    }
}

/// A segment command (`LC_SEGMENT` or `LC_SEGMENT_64`).
#[derive(Clone, Copy, Debug)]
pub struct Segment<'a> {
    /// `segname`, without padding. Empty in relocatable objects.
    pub name: &'a [u8],
    /// `vmaddr`.
    pub vmaddr: u64,
    /// `vmsize`.
    pub vmsize: u64,
    /// `fileoff`.
    pub fileoff: u64,
    /// `filesize`.
    pub filesize: u64,
    /// `maxprot`.
    pub maxprot: u32,
    /// `initprot`.
    pub initprot: u32,
    /// `flags`.
    pub flags: u32,
    /// The section records.
    pub sections: SectionTable<'a>,
}

/// `LC_SYMTAB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SymtabCommand {
    /// File offset of the `nlist` array.
    pub symoff: u32,
    /// Number of symbols.
    pub nsyms: u32,
    /// File offset of the string table.
    pub stroff: u32,
    /// Size of the string table.
    pub strsize: u32,
}

/// `LC_DYSYMTAB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DysymtabCommand {
    /// Index of the first local symbol.
    pub ilocalsym: u32,
    /// Number of local symbols.
    pub nlocalsym: u32,
    /// Index of the first external defined symbol.
    pub iextdefsym: u32,
    /// Number of external defined symbols.
    pub nextdefsym: u32,
    /// Index of the first undefined symbol.
    pub iundefsym: u32,
    /// Number of undefined symbols.
    pub nundefsym: u32,
    /// Table of contents offset.
    pub tocoff: u32,
    /// Table of contents entries.
    pub ntoc: u32,
    /// Module table offset.
    pub modtaboff: u32,
    /// Module table entries.
    pub nmodtab: u32,
    /// External reference table offset.
    pub extrefsymoff: u32,
    /// External reference table entries.
    pub nextrefsyms: u32,
    /// Indirect symbol table offset.
    pub indirectsymoff: u32,
    /// Indirect symbol table entries.
    pub nindirectsyms: u32,
    /// External relocation entries offset.
    pub extreloff: u32,
    /// External relocation entries.
    pub nextrel: u32,
    /// Local relocation entries offset.
    pub locreloff: u32,
    /// Local relocation entries.
    pub nlocrel: u32,
}

/// A version packed as `xxxx.yy.zz` (16, 8 and 8 bits), as in dylib
/// versions and `LC_BUILD_VERSION`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct PackedVersion(pub u32);

impl PackedVersion {
    /// Builds a version from its components; out-of-range components are
    /// truncated.
    #[must_use]
    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self(((major & 0xffff) << 16) | ((minor & 0xff) << 8) | (patch & 0xff))
    }

    /// Major version.
    #[must_use]
    pub fn major(self) -> u32 {
        self.0 >> 16
    }

    /// Minor version.
    #[must_use]
    pub fn minor(self) -> u32 {
        (self.0 >> 8) & 0xff
    }

    /// Patch version.
    #[must_use]
    pub fn patch(self) -> u32 {
        self.0 & 0xff
    }

    /// Parses `X[.Y[.Z]]`, with `X` < 65536 and `Y`, `Z` < 256.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split('.');
        let mut component = |max: u32, required: bool| -> Option<u32> {
            match parts.next() {
                None if !required => Some(0),
                None => None,
                Some(p) => {
                    if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                        return None;
                    }
                    p.parse::<u32>().ok().filter(|&v| v <= max)
                }
            }
        };
        let major = component(0xffff, true)?;
        let minor = component(0xff, false)?;
        let patch = component(0xff, false)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self::new(major, minor, patch))
    }
}

impl std::fmt::Display for PackedVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major(), self.minor())?;
        if self.patch() != 0 {
            write!(f, ".{}", self.patch())?;
        }
        Ok(())
    }
}

/// `LC_BUILD_VERSION` or one of the `LC_VERSION_MIN_*` commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildVersion<'a> {
    /// The command it came from.
    pub cmd: u32,
    /// `PLATFORM_*` (derived from the command for `LC_VERSION_MIN_*`).
    pub platform: u32,
    /// Minimum OS version.
    pub minos: PackedVersion,
    /// SDK version.
    pub sdk: PackedVersion,
    /// Raw `build_tool_version` records (tool, version), 8 bytes each.
    pub tools: &'a [u8],
}

impl BuildVersion<'_> {
    /// Iterates over (`TOOL_*`, packed version) pairs.
    pub fn tools(&self, endian: Endian) -> impl Iterator<Item = (u32, PackedVersion)> + '_ {
        self.tools.as_chunks::<8>().0.iter().map(move |record| {
            (
                endian.u32(record, 0).unwrap_or(0),
                PackedVersion(endian.u32(record, 4).unwrap_or(0)),
            )
        })
    }
}

/// A dylib reference: `LC_ID_DYLIB`, `LC_LOAD_DYLIB` and friends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DylibCommand<'a> {
    /// The command.
    pub cmd: u32,
    /// Install name.
    pub name: &'a [u8],
    /// Build timestamp.
    pub timestamp: u32,
    /// Current version.
    pub current_version: PackedVersion,
    /// Compatibility version.
    pub compatibility_version: PackedVersion,
}

/// How a dylib dependency is loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DylibLoadKind {
    /// `LC_LOAD_DYLIB`.
    Regular,
    /// `LC_LOAD_WEAK_DYLIB`.
    Weak,
    /// `LC_REEXPORT_DYLIB`.
    Reexport,
    /// `LC_LOAD_UPWARD_DYLIB`.
    Upward,
    /// `LC_LAZY_LOAD_DYLIB`.
    Lazy,
}

impl DylibLoadKind {
    /// The load kind of a dependency command, or `None` for other commands.
    #[must_use]
    pub fn from_cmd(cmd: u32) -> Option<Self> {
        Some(match cmd {
            LC_LOAD_DYLIB => Self::Regular,
            LC_LOAD_WEAK_DYLIB => Self::Weak,
            LC_REEXPORT_DYLIB => Self::Reexport,
            LC_LOAD_UPWARD_DYLIB => Self::Upward,
            LC_LAZY_LOAD_DYLIB => Self::Lazy,
            _ => return None,
        })
    }
}

/// `LC_DYLD_INFO` / `LC_DYLD_INFO_ONLY`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DyldInfoCommand {
    /// Rebase opcodes offset.
    pub rebase_off: u32,
    /// Rebase opcodes size.
    pub rebase_size: u32,
    /// Bind opcodes offset.
    pub bind_off: u32,
    /// Bind opcodes size.
    pub bind_size: u32,
    /// Weak bind opcodes offset.
    pub weak_bind_off: u32,
    /// Weak bind opcodes size.
    pub weak_bind_size: u32,
    /// Lazy bind opcodes offset.
    pub lazy_bind_off: u32,
    /// Lazy bind opcodes size.
    pub lazy_bind_size: u32,
    /// Export trie offset.
    pub export_off: u32,
    /// Export trie size.
    pub export_size: u32,
}

/// A `linkedit_data_command`: an offset and size in `__LINKEDIT` (or in the
/// object, for `LC_DATA_IN_CODE` and `LC_LINKER_OPTIMIZATION_HINT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkeditData {
    /// The command.
    pub cmd: u32,
    /// File offset of the data.
    pub dataoff: u32,
    /// Size of the data.
    pub datasize: u32,
}

/// What a `LC_LINKER_OPTION` string pair asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkerOptionHint<'a> {
    /// `-lfoo` or `-l foo`: link against library `foo`.
    Library(&'a [u8]),
    /// `-framework Foo`.
    Framework(&'a [u8]),
    /// Anything else (`-weak_framework Foo`, `-needed-lfoo`, …).
    Other,
}

/// One `data_in_code_entry`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataInCodeEntry {
    /// Offset from the start of the section's segment (in objects: the
    /// start of the section contents' address space).
    pub offset: u32,
    /// Length in bytes.
    pub length: u16,
    /// `DICE_KIND_*`.
    pub kind: u16,
}

/// One entry of the linker optimization hint stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptimizationHint {
    /// `LOH_ARM64_*` kind.
    pub kind: u64,
    /// Number of addresses (at most 3 for known kinds).
    pub count: u8,
    /// The addresses; entries past `count` are zero.
    pub addresses: [u64; 3],
}

impl OptimizationHint {
    /// The addresses in use.
    #[must_use]
    pub fn addresses(&self) -> &[u64] {
        self.addresses.get(..usize::from(self.count)).unwrap_or(&[])
    }
}

/// Iterator over the ULEB128 stream of `LC_LINKER_OPTIMIZATION_HINT`.
///
/// Each entry is `kind`, `count`, then `count` addresses. Iteration ends at
/// the end of the data or at a zero kind (padding). A truncated entry, or
/// one with more than three addresses, ends iteration with an error.
#[derive(Clone, Debug)]
pub struct OptimizationHintIter<'a> {
    data: &'a [u8],
    pos: usize,
    file_offset: u64,
    source: Source<'a>,
}

impl<'a> OptimizationHintIter<'a> {
    /// Iterates over `data`, found at `file_offset`.
    #[must_use]
    pub fn new(data: &'a [u8], file_offset: u64, source: Source<'a>) -> Self {
        Self {
            data,
            pos: 0,
            file_offset,
            source,
        }
    }
}

impl Iterator for OptimizationHintIter<'_> {
    type Item = Result<OptimizationHint>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        let start = self.pos;
        let mut decode = || -> Option<Option<OptimizationHint>> {
            let kind = read_uleb(self.data, &mut self.pos)?;
            if kind == 0 {
                return Some(None);
            }
            let count = read_uleb(self.data, &mut self.pos)?;
            let count = u8::try_from(count).ok().filter(|&c| c <= 3)?;
            let mut addresses = [0u64; 3];
            for slot in addresses.iter_mut().take(usize::from(count)) {
                *slot = read_uleb(self.data, &mut self.pos)?;
            }
            Some(Some(OptimizationHint {
                kind,
                count,
                addresses,
            }))
        };
        match decode() {
            Some(Some(hint)) => Some(Ok(hint)),
            Some(None) => {
                self.pos = self.data.len();
                None
            }
            None => {
                self.pos = self.data.len();
                Some(Err(self.source.malformed(
                    self.file_offset.saturating_add(to_u64(start)),
                    "linker optimization hint",
                )))
            }
        }
    }
}

impl<'a> LoadCommand<'a> {
    /// Whether the file has a 64-bit header.
    #[must_use]
    pub fn is64(&self) -> bool {
        self.is64
    }

    /// Byte order of the file.
    #[must_use]
    pub fn endian(&self) -> Endian {
        self.endian
    }

    /// The command's name, such as `"LC_SYMTAB"`.
    #[must_use]
    pub fn name(&self) -> Option<&'static str> {
        load_command_name(self.cmd)
    }

    #[cold]
    fn error(&self, what: &str) -> crate::Error {
        let name = self
            .name()
            .map_or_else(|| format!("load command {:#x}", self.cmd), str::to_owned);
        self.source
            .malformed(self.offset, format!("{name} ({what})"))
    }

    fn u32_at(&self, offset: usize) -> Result<u32> {
        self.endian
            .u32(self.data, offset)
            .ok_or_else(|| self.error("truncated"))
    }

    fn u64_at(&self, offset: usize) -> Result<u64> {
        self.endian
            .u64(self.data, offset)
            .ok_or_else(|| self.error("truncated"))
    }

    /// Reads an `lc_str` whose offset is stored at `field`. The string must
    /// start after the command's `fixed` bytes of fixed fields.
    fn lc_str(&self, field: usize, fixed: usize) -> Result<&'a [u8]> {
        let offset = self.u32_at(field)?;
        if self.data.len() < fixed {
            return Err(self.error("truncated"));
        }
        usize::try_from(offset)
            .ok()
            .filter(|&o| o >= fixed)
            .and_then(|o| self.data.get(o..))
            .and_then(cstr)
            .ok_or_else(|| self.error("string offset or terminator"))
    }

    /// Decodes `LC_SEGMENT` / `LC_SEGMENT_64`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` for other commands or if the command is
    /// too small for its sections.
    pub fn segment(&self) -> Result<Segment<'a>> {
        let (is64, header, record) = match self.cmd {
            LC_SEGMENT_64 => (true, 72usize, SECTION_64_SIZE),
            LC_SEGMENT => (false, 56usize, SECTION_SIZE),
            _ => return Err(self.error("not a segment command")),
        };
        if self.data.len() < header {
            return Err(self.error("truncated"));
        }
        let name = fixed_name(self.data.get(8..24).unwrap_or(&[]));
        let (vmaddr, vmsize, fileoff, filesize, rest) = if is64 {
            (
                self.u64_at(24)?,
                self.u64_at(32)?,
                self.u64_at(40)?,
                self.u64_at(48)?,
                56,
            )
        } else {
            (
                u64::from(self.u32_at(24)?),
                u64::from(self.u32_at(28)?),
                u64::from(self.u32_at(32)?),
                u64::from(self.u32_at(36)?),
                40,
            )
        };
        let maxprot = self.u32_at(rest)?;
        let initprot = self.u32_at(rest.saturating_add(4))?;
        let nsects = self.u32_at(rest.saturating_add(8))?;
        let flags = self.u32_at(rest.saturating_add(12))?;
        let table = usize::try_from(nsects)
            .ok()
            .and_then(|n| n.checked_mul(record))
            .and_then(|size| self.data.get(header..)?.get(..size))
            .ok_or_else(|| self.error("section headers extend past the command"))?;
        Ok(Segment {
            name,
            vmaddr,
            vmsize,
            fileoff,
            filesize,
            maxprot,
            initprot,
            flags,
            sections: SectionTable {
                data: table,
                file_offset: self.offset.saturating_add(to_u64(header)),
                endian: self.endian,
                is64,
            },
        })
    }

    /// Decodes `LC_SYMTAB`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the command is truncated.
    pub fn symtab(&self) -> Result<SymtabCommand> {
        Ok(SymtabCommand {
            symoff: self.u32_at(8)?,
            nsyms: self.u32_at(12)?,
            stroff: self.u32_at(16)?,
            strsize: self.u32_at(20)?,
        })
    }

    /// Decodes `LC_DYSYMTAB`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the command is truncated.
    pub fn dysymtab(&self) -> Result<DysymtabCommand> {
        let mut fields = [0u32; 18];
        for (i, field) in fields.iter_mut().enumerate() {
            *field = self.u32_at(i.saturating_mul(4).saturating_add(8))?;
        }
        let [
            ilocalsym,
            nlocalsym,
            iextdefsym,
            nextdefsym,
            iundefsym,
            nundefsym,
            tocoff,
            ntoc,
            modtaboff,
            nmodtab,
            extrefsymoff,
            nextrefsyms,
            indirectsymoff,
            nindirectsyms,
            extreloff,
            nextrel,
            locreloff,
            nlocrel,
        ] = fields;
        Ok(DysymtabCommand {
            ilocalsym,
            nlocalsym,
            iextdefsym,
            nextdefsym,
            iundefsym,
            nundefsym,
            tocoff,
            ntoc,
            modtaboff,
            nmodtab,
            extrefsymoff,
            nextrefsyms,
            indirectsymoff,
            nindirectsyms,
            extreloff,
            nextrel,
            locreloff,
            nlocrel,
        })
    }

    /// Decodes `LC_BUILD_VERSION` or `LC_VERSION_MIN_*`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` for other commands, or if the command or
    /// its tool list is truncated.
    pub fn build_version(&self) -> Result<BuildVersion<'a>> {
        let platform = match self.cmd {
            LC_BUILD_VERSION => {
                let ntools = self.u32_at(20)?;
                let tools = usize::try_from(ntools)
                    .ok()
                    .and_then(|n| n.checked_mul(8))
                    .and_then(|size| self.data.get(24..)?.get(..size))
                    .ok_or_else(|| self.error("tools extend past the command"))?;
                return Ok(BuildVersion {
                    cmd: self.cmd,
                    platform: self.u32_at(8)?,
                    minos: PackedVersion(self.u32_at(12)?),
                    sdk: PackedVersion(self.u32_at(16)?),
                    tools,
                });
            }
            LC_VERSION_MIN_MACOSX => PLATFORM_MACOS,
            LC_VERSION_MIN_IPHONEOS => PLATFORM_IOS,
            LC_VERSION_MIN_TVOS => PLATFORM_TVOS,
            LC_VERSION_MIN_WATCHOS => PLATFORM_WATCHOS,
            _ => return Err(self.error("not a version command")),
        };
        Ok(BuildVersion {
            cmd: self.cmd,
            platform,
            minos: PackedVersion(self.u32_at(8)?),
            sdk: PackedVersion(self.u32_at(12)?),
            tools: &[],
        })
    }

    /// Whether this is `LC_BUILD_VERSION` or `LC_VERSION_MIN_*`.
    #[must_use]
    pub fn is_version(&self) -> bool {
        matches!(
            self.cmd,
            LC_BUILD_VERSION
                | LC_VERSION_MIN_MACOSX
                | LC_VERSION_MIN_IPHONEOS
                | LC_VERSION_MIN_TVOS
                | LC_VERSION_MIN_WATCHOS
        )
    }

    /// Decodes a `dylib_command` (`LC_ID_DYLIB`, `LC_LOAD_DYLIB`, …).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` for other commands, or if the command is
    /// truncated or its name is not inside it.
    pub fn dylib(&self) -> Result<DylibCommand<'a>> {
        if self.cmd != LC_ID_DYLIB && DylibLoadKind::from_cmd(self.cmd).is_none() {
            return Err(self.error("not a dylib command"));
        }
        Ok(DylibCommand {
            cmd: self.cmd,
            name: self.lc_str(8, 24)?,
            timestamp: self.u32_at(12)?,
            current_version: PackedVersion(self.u32_at(16)?),
            compatibility_version: PackedVersion(self.u32_at(20)?),
        })
    }

    /// Decodes a command made of a single `lc_str`: `LC_RPATH`,
    /// `LC_SUB_FRAMEWORK`, `LC_SUB_CLIENT`, `LC_SUB_UMBRELLA`,
    /// `LC_SUB_LIBRARY`, `LC_LOAD_DYLINKER`, `LC_ID_DYLINKER`,
    /// `LC_DYLD_ENVIRONMENT`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the string is not inside the command.
    pub fn string(&self) -> Result<&'a [u8]> {
        self.lc_str(8, 12)
    }

    /// Decodes `LC_DYLD_INFO` / `LC_DYLD_INFO_ONLY`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the command is truncated.
    pub fn dyld_info(&self) -> Result<DyldInfoCommand> {
        Ok(DyldInfoCommand {
            rebase_off: self.u32_at(8)?,
            rebase_size: self.u32_at(12)?,
            bind_off: self.u32_at(16)?,
            bind_size: self.u32_at(20)?,
            weak_bind_off: self.u32_at(24)?,
            weak_bind_size: self.u32_at(28)?,
            lazy_bind_off: self.u32_at(32)?,
            lazy_bind_size: self.u32_at(36)?,
            export_off: self.u32_at(40)?,
            export_size: self.u32_at(44)?,
        })
    }

    /// Decodes a `linkedit_data_command` (`LC_DATA_IN_CODE`,
    /// `LC_LINKER_OPTIMIZATION_HINT`, `LC_DYLD_EXPORTS_TRIE`,
    /// `LC_DYLD_CHAINED_FIXUPS`, `LC_FUNCTION_STARTS`, …).
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the command is truncated.
    pub fn linkedit_data(&self) -> Result<LinkeditData> {
        Ok(LinkeditData {
            cmd: self.cmd,
            dataoff: self.u32_at(8)?,
            datasize: self.u32_at(12)?,
        })
    }

    /// The strings of `LC_LINKER_OPTION`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the command is truncated or has fewer
    /// NUL-terminated strings than its `count`.
    pub fn linker_option_strings(&self) -> Result<Vec<&'a [u8]>> {
        let count = self.u32_at(8)?;
        let mut rest = self.data.get(12..).unwrap_or(&[]);
        let mut strings = Vec::with_capacity(count.min(16) as usize);
        for _ in 0..count {
            let s = cstr(rest).ok_or_else(|| self.error("strings (fewer than count)"))?;
            rest = rest.get(s.len().saturating_add(1)..).unwrap_or(&[]);
            strings.push(s);
        }
        Ok(strings)
    }
}

/// Interprets the strings of one `LC_LINKER_OPTION` command.
#[must_use]
pub fn linker_option_hint<'a>(strings: &[&'a [u8]]) -> LinkerOptionHint<'a> {
    match *strings {
        [single] if single.starts_with(b"-l") && single.len() > 2 => {
            LinkerOptionHint::Library(single.get(2..).unwrap_or(&[]))
        }
        [flag, name] if flag == b"-l" => LinkerOptionHint::Library(name),
        [flag, name] if flag == b"-framework" => LinkerOptionHint::Framework(name),
        _ => LinkerOptionHint::Other,
    }
}

/// Decodes a `data_in_code_entry` table.
pub fn data_in_code_entries(
    data: &[u8],
    endian: Endian,
) -> impl ExactSizeIterator<Item = DataInCodeEntry> + '_ {
    data.as_chunks::<8>()
        .0
        .iter()
        .map(move |record| DataInCodeEntry {
            offset: endian.u32(record, 0).unwrap_or(0),
            length: endian.u16(record, 4).unwrap_or(0),
            kind: endian.u16(record, 6).unwrap_or(0),
        })
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn packed_versions() {
        assert_eq!(
            PackedVersion::parse("1292.100.5"),
            Some(PackedVersion(0x050c_6405))
        );
        assert_eq!(PackedVersion::parse("1"), Some(PackedVersion(0x1_0000)));
        assert_eq!(PackedVersion::parse("1.2"), Some(PackedVersion(0x1_0200)));
        assert_eq!(PackedVersion::parse("1.256"), None);
        assert_eq!(PackedVersion::parse("1.2.3.4"), None);
        assert_eq!(PackedVersion::parse("x"), None);
        assert_eq!(PackedVersion::parse(""), None);
        assert_eq!(PackedVersion(0x050c_6405).to_string(), "1292.100.5");
        assert_eq!(PackedVersion(0x1_0000).to_string(), "1.0");
    }

    #[test]
    fn hints() {
        let one: &[&[u8]] = &[b"-lSystem"];
        assert_eq!(
            linker_option_hint(one),
            LinkerOptionHint::Library(b"System")
        );
        let two: &[&[u8]] = &[b"-framework", b"Foundation"];
        assert_eq!(
            linker_option_hint(two),
            LinkerOptionHint::Framework(b"Foundation")
        );
        let other: &[&[u8]] = &[b"-weak_framework", b"Foo"];
        assert_eq!(linker_option_hint(other), LinkerOptionHint::Other);
    }
}
