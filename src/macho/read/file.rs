//! The Mach-O header and load command table, shared by objects and dylibs.

use super::arch::Arch;
use super::bytes::{Endian, Source, to_u64};
use super::commands::{LoadCommand, LoadCommandIter};
use super::consts::{MH_CIGAM, MH_CIGAM_64, MH_MAGIC, MH_MAGIC_64, MH_SUBSECTIONS_VIA_SYMBOLS};
use crate::error::Result;

/// A decoded `mach_header` or `mach_header_64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachHeader {
    /// `magic`, in file byte order ([`MH_MAGIC`] or [`MH_MAGIC_64`]).
    pub magic: u32,
    /// `cputype`.
    pub cpu_type: u32,
    /// `cpusubtype`, with capability bits.
    pub cpu_subtype: u32,
    /// `filetype` (`MH_OBJECT`, `MH_DYLIB`, …).
    pub file_type: u32,
    /// `ncmds`.
    pub ncmds: u32,
    /// `sizeofcmds`.
    pub sizeofcmds: u32,
    /// `flags` (`MH_SUBSECTIONS_VIA_SYMBOLS`, …).
    pub flags: u32,
    /// `reserved` (64-bit headers only; zero otherwise).
    pub reserved: u32,
}

impl MachHeader {
    /// Whether this is a 64-bit header.
    #[must_use]
    pub fn is64(&self) -> bool {
        self.magic == MH_MAGIC_64
    }

    /// Size of the header: 28 bytes, or 32 for 64-bit.
    #[must_use]
    pub fn size(&self) -> usize {
        if self.is64() { 32 } else { 28 }
    }

    /// The architecture (subtype capability bits masked off).
    #[must_use]
    pub fn arch(&self) -> Arch {
        Arch::new(self.cpu_type, self.cpu_subtype)
    }

    /// Whether `MH_SUBSECTIONS_VIA_SYMBOLS` is set.
    #[must_use]
    pub fn subsections_via_symbols(&self) -> bool {
        self.flags & MH_SUBSECTIONS_VIA_SYMBOLS != 0
    }
}

/// A Mach-O file of any type: its header and load commands.
///
/// Parsing checks that the header and the load command area fit in the
/// data; each command is bounds-checked as it is iterated.
#[derive(Clone, Copy, Debug)]
pub struct MachOFile<'a> {
    data: &'a [u8],
    header: MachHeader,
    endian: Endian,
    commands: &'a [u8],
    source: Source<'a>,
}

impl<'a> MachOFile<'a> {
    /// Parses the header of a thin (non-universal) Mach-O file.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the magic is not a Mach-O magic or the
    /// header or load commands do not fit.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        let raw_magic = Endian::LITTLE
            .u32(data, 0)
            .ok_or_else(|| source.malformed(0, "Mach-O header (truncated)"))?;
        // `MH_MAGIC` read little-endian means a little-endian file.
        let (endian, is64) = match raw_magic {
            MH_MAGIC => (Endian::LITTLE, false),
            MH_MAGIC_64 => (Endian::LITTLE, true),
            MH_CIGAM => (Endian::BIG, false),
            MH_CIGAM_64 => (Endian::BIG, true),
            _ => return Err(source.malformed(0, "Mach-O magic")),
        };
        let header_size = if is64 { 32 } else { 28 };
        let field = |offset: usize| {
            endian
                .u32(data, offset)
                .ok_or_else(|| source.malformed(0, "Mach-O header (truncated)"))
        };
        let header = MachHeader {
            magic: if is64 { MH_MAGIC_64 } else { MH_MAGIC },
            cpu_type: field(4)?,
            cpu_subtype: field(8)?,
            file_type: field(12)?,
            ncmds: field(16)?,
            sizeofcmds: field(20)?,
            flags: field(24)?,
            reserved: if is64 { field(28)? } else { 0 },
        };
        let commands = usize::try_from(header.sizeofcmds)
            .ok()
            .and_then(|size| data.get(header_size..)?.get(..size))
            .ok_or_else(|| {
                source.malformed(20, "load commands (sizeofcmds extends past end of file)")
            })?;
        Ok(Self {
            data,
            header,
            endian,
            commands,
            source,
        })
    }

    /// The whole file.
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The header.
    #[must_use]
    pub fn header(&self) -> &MachHeader {
        &self.header
    }

    /// Byte order of the file.
    #[must_use]
    pub fn endian(&self) -> Endian {
        self.endian
    }

    /// Whether the file has a 64-bit header.
    #[must_use]
    pub fn is64(&self) -> bool {
        self.header.is64()
    }

    /// The error context.
    #[must_use]
    pub fn source(&self) -> Source<'a> {
        self.source
    }

    /// Iterates over the load commands.
    #[must_use]
    pub fn load_commands(&self) -> LoadCommandIter<'a> {
        LoadCommandIter::new(
            self.commands,
            to_u64(self.header.size()),
            self.header.ncmds,
            self.endian,
            self.is64(),
            self.source,
        )
    }

    /// Returns the bytes at `offset..offset + size` in the file, naming
    /// `what` in the error.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` when the range is outside the file.
    pub fn bytes(&self, offset: u64, size: u64, what: &str) -> Result<&'a [u8]> {
        super::bytes::subslice(self.data, offset, size).ok_or_else(|| {
            self.source.malformed(
                offset,
                format!("{what} (size {size:#x} extends past end of file)"),
            )
        })
    }

    /// Finds the first load command with `cmd`.
    ///
    /// # Errors
    ///
    /// Returns the first malformed load command met before it.
    pub fn find_command(&self, cmd: u32) -> Result<Option<LoadCommand<'a>>> {
        for command in self.load_commands() {
            let command = command?;
            if command.cmd == cmd {
                return Ok(Some(command));
            }
        }
        Ok(None)
    }
}
