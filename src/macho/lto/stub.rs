//! Stand-ins for bitcode during symbol resolution, and `ar` archives
//! rebuilt in memory.
//!
//! A stand-in is an `MH_OBJECT` with no contents, whose symbol table
//! mirrors a bitcode module's: definitions are absolute symbols (weak,
//! private extern or common as the module says), references are undefined.
//! Resolution treats it like the object LTO will produce, so archive
//! members are extracted as ld64 would extract them. Its `LC_LINKER_OPTION`
//! commands carry the module's auto-linking requests, and an empty
//! `__objc_catlist` section marks a module with an Objective-C category for
//! `-ObjC`.

#![deny(clippy::arithmetic_side_effects)]

use crate::macho::buf::{pad_to, push_name16, push16, push32, push64};
use crate::macho::read::consts::{
    LC_LINKER_OPTION, LC_SEGMENT_64, LC_SYMTAB, MH_MAGIC_64, MH_OBJECT, MH_SUBSECTIONS_VIA_SYMBOLS,
    N_ABS, N_EXT, N_PEXT, N_UNDF, N_WEAK_DEF, N_WEAK_REF, S_REGULAR,
};

/// How a stand-in symbol takes part in resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StubKind {
    /// A definition.
    Defined {
        /// A weak definition (`linkonce`, `weak`).
        weak: bool,
        /// Hidden visibility: a private extern.
        hidden: bool,
    },
    /// A tentative definition.
    Common,
    /// A reference.
    Undefined {
        /// A weak reference.
        weak: bool,
    },
}

/// One symbol of a stand-in.
#[derive(Clone, Copy, Debug)]
pub(super) struct StubSymbol<'a> {
    /// The name.
    pub(super) name: &'a [u8],
    /// Its kind.
    pub(super) kind: StubKind,
}

/// Builds a stand-in object for a module of CPU `cpu` (type, subtype).
///
/// `linker_options` holds the strings of each `LC_LINKER_OPTION` command.
pub(super) fn object(
    cpu: (u32, u32),
    symbols: &[StubSymbol<'_>],
    linker_options: &[Vec<&[u8]>],
    objc_category: bool,
) -> Vec<u8> {
    let mut commands = Vec::new();
    let mut ncmds = 0u32;
    if objc_category {
        // LC_SEGMENT_64 with an empty `__DATA,__objc_catlist`.
        push32(&mut commands, LC_SEGMENT_64);
        push32(&mut commands, 72 + 80);
        push_name16(&mut commands, b"");
        for _ in 0..4 {
            push64(&mut commands, 0); // vmaddr, vmsize, fileoff, filesize
        }
        push32(&mut commands, 3); // maxprot
        push32(&mut commands, 3); // initprot
        push32(&mut commands, 1); // nsects
        push32(&mut commands, 0); // flags
        push_name16(&mut commands, b"__objc_catlist");
        push_name16(&mut commands, b"__DATA");
        push64(&mut commands, 0); // addr
        push64(&mut commands, 0); // size
        push32(&mut commands, 0); // offset
        push32(&mut commands, 3); // align
        push32(&mut commands, 0); // reloff
        push32(&mut commands, 0); // nreloc
        push32(&mut commands, S_REGULAR);
        push32(&mut commands, 0); // reserved1
        push32(&mut commands, 0); // reserved2
        push32(&mut commands, 0); // reserved3
        ncmds = ncmds.saturating_add(1);
    }
    for strings in linker_options {
        let mut body = Vec::new();
        for string in strings {
            body.extend_from_slice(string);
            body.push(0);
        }
        pad_to(&mut body, 8);
        let size = u32::try_from(body.len().saturating_add(12)).unwrap_or(u32::MAX);
        let mut command = Vec::new();
        push32(&mut command, LC_LINKER_OPTION);
        push32(&mut command, size.next_multiple_of(8));
        push32(&mut command, u32::try_from(strings.len()).unwrap_or(0));
        command.extend_from_slice(&body);
        pad_to(&mut command, 8);
        commands.extend_from_slice(&command);
        ncmds = ncmds.saturating_add(1);
    }

    let mut strings = vec![b' ', 0];
    let mut table = Vec::with_capacity(symbols.len().saturating_mul(16));
    for symbol in symbols {
        let strx = u32::try_from(strings.len()).unwrap_or(0);
        strings.extend_from_slice(symbol.name);
        strings.push(0);
        let (n_type, n_desc, n_value) = match symbol.kind {
            StubKind::Defined { weak, hidden } => (
                N_ABS | N_EXT | if hidden { N_PEXT } else { 0 },
                if weak { N_WEAK_DEF } else { 0 },
                0,
            ),
            // The size is unknown before code generation; any size makes
            // it a tentative definition.
            StubKind::Common => (N_UNDF | N_EXT, 0, 1),
            StubKind::Undefined { weak } => (N_UNDF | N_EXT, if weak { N_WEAK_REF } else { 0 }, 0),
        };
        push32(&mut table, strx);
        table.push(n_type);
        table.push(0);
        push16(&mut table, n_desc);
        push64(&mut table, n_value);
    }
    pad_to(&mut strings, 8);

    let symtab_command_size = 24usize;
    let header_size = 32usize;
    let commands_size = commands.len().saturating_add(symtab_command_size);
    let symoff = header_size.saturating_add(commands_size);
    let stroff = symoff.saturating_add(table.len());
    let as_u32 = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);

    let mut out = Vec::with_capacity(stroff.saturating_add(strings.len()));
    push32(&mut out, MH_MAGIC_64);
    push32(&mut out, cpu.0);
    push32(&mut out, cpu.1);
    push32(&mut out, MH_OBJECT);
    push32(&mut out, ncmds.saturating_add(1));
    push32(&mut out, as_u32(commands_size));
    push32(&mut out, MH_SUBSECTIONS_VIA_SYMBOLS);
    push32(&mut out, 0);
    out.extend_from_slice(&commands);
    push32(&mut out, LC_SYMTAB);
    push32(&mut out, as_u32(symtab_command_size));
    push32(&mut out, as_u32(symoff));
    push32(&mut out, as_u32(symbols.len()));
    push32(&mut out, as_u32(stroff));
    push32(&mut out, as_u32(strings.len()));
    out.extend_from_slice(&table);
    out.extend_from_slice(&strings);
    out
}

/// A member of an archive built by [`archive`].
#[derive(Clone, Copy, Debug)]
pub(super) struct ArchiveMember<'a> {
    /// The member name.
    pub(super) name: &'a [u8],
    /// The modification time field of the original header, for the debug
    /// map.
    pub(super) date: u64,
    /// The contents.
    pub(super) data: &'a [u8],
}

/// Writes a BSD `ar` archive of `members` (names in `#1/<length>` form,
/// contents 8-byte aligned, no symbol table: the Mach-O linker reads the
/// members' symbols itself). Returns the archive and the offset of each
/// member's contents in it.
pub(super) fn archive(members: &[ArchiveMember<'_>]) -> (Vec<u8>, Vec<usize>) {
    let mut out = b"!<arch>\n".to_vec();
    let mut offsets = Vec::with_capacity(members.len());
    for member in members {
        let mut name = member.name.to_vec();
        name.push(0);
        // The header is 60 bytes after an 8-aligned offset: pad the name so
        // the contents start 8-aligned.
        let unpadded = out.len().saturating_add(60).saturating_add(name.len());
        name.resize(
            name.len()
                .saturating_add(unpadded.next_multiple_of(8).saturating_sub(unpadded)),
            0,
        );
        let size = name.len().saturating_add(member.data.len());
        let mut header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            format!("#1/{}", name.len()),
            member.date,
            0,
            0,
            "100644",
            size
        )
        .into_bytes();
        header.truncate(60);
        out.extend_from_slice(&header);
        out.extend_from_slice(&name);
        offsets.push(out.len());
        out.extend_from_slice(member.data);
        // Headers start at even offsets.
        if out.len() & 1 == 1 {
            out.push(b'\n');
        }
    }
    (out, offsets)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::input::archive::Archive;
    use crate::macho::object::LinkObject;
    use crate::macho::read::consts::CPU_TYPE_ARM64;
    use crate::macho::read::{LinkerOptionHint, ObjectFile, Source};
    use crate::symbols::{DefinitionKind, SymbolUse};

    #[test]
    fn stand_in_resolves_like_the_module() {
        let symbols = [
            StubSymbol {
                name: b"_main",
                kind: StubKind::Defined {
                    weak: false,
                    hidden: false,
                },
            },
            StubSymbol {
                name: b"_inline",
                kind: StubKind::Defined {
                    weak: true,
                    hidden: true,
                },
            },
            StubSymbol {
                name: b"_tentative",
                kind: StubKind::Common,
            },
            StubSymbol {
                name: b"_puts",
                kind: StubKind::Undefined { weak: false },
            },
            StubSymbol {
                name: b"_maybe",
                kind: StubKind::Undefined { weak: true },
            },
        ];
        let data = object(
            (CPU_TYPE_ARM64, 0),
            &symbols,
            &[vec![b"-lz"], vec![b"-framework", b"Foundation"]],
            true,
        );
        let file = ObjectFile::parse(&data, Source::new(Path::new("stub.o"))).unwrap();
        assert!(
            file.sections()
                .iter()
                .any(|s| s.sectname == b"__objc_catlist")
        );
        let hints = file.linker_option_hints().unwrap();
        assert_eq!(
            hints,
            [
                LinkerOptionHint::Library(b"z"),
                LinkerOptionHint::Framework(b"Foundation")
            ]
        );
        let object = LinkObject::new(file).unwrap();
        let names: Vec<&[u8]> = object.names.iter().map(|n| n.bytes()).collect();
        assert_eq!(
            names,
            [
                b"_main".as_slice(),
                b"_inline",
                b"_tentative",
                b"_puts",
                b"_maybe"
            ]
        );
        assert!(matches!(
            object.uses[0],
            SymbolUse::Definition {
                kind: DefinitionKind::Regular,
                ..
            }
        ));
        assert!(matches!(
            object.uses[1],
            SymbolUse::Definition {
                kind: DefinitionKind::Weak,
                aux: 1
            }
        ));
        assert!(matches!(
            object.uses[2],
            SymbolUse::Definition {
                kind: DefinitionKind::Common,
                ..
            }
        ));
        assert_eq!(object.uses[3], SymbolUse::Reference { weak: false });
        assert_eq!(object.uses[4], SymbolUse::Reference { weak: true });
        let defined = LinkObject::defined_names(&object.file).unwrap();
        assert_eq!(defined.len(), 3);
    }

    #[test]
    fn rebuilt_archive_reads_back() {
        let members = [
            ArchiveMember {
                name: b"a.o",
                date: 1234,
                data: b"first",
            },
            ArchiveMember {
                name: b"a_rather_long_member_name.o",
                date: 0,
                data: b"second member",
            },
        ];
        let (data, offsets) = archive(&members);
        let archive = Archive::parse(Path::new("lib.a"), &data).unwrap();
        let read: Vec<_> = archive.members().map(Result::unwrap).collect();
        assert_eq!(read.len(), 2);
        for ((member, original), offset) in read.iter().zip(&members).zip(&offsets) {
            assert_eq!(member.name, original.name);
            let bytes = member.bytes().unwrap();
            assert_eq!(bytes, original.data);
            assert_eq!(bytes.as_ptr(), data[*offset..].as_ptr());
            assert_eq!(offset % 8, 0);
        }
        let date = &data[read[0].header_offset as usize + 16..][..12];
        assert_eq!(std::str::from_utf8(date).unwrap().trim(), "1234");
    }
}
