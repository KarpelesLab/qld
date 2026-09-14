//! Writing an import library (`--out-implib`) and a module-definition file
//! (`--output-def`).
//!
//! qld writes the *long* form GNU `dlltool` produces, because that is what
//! MinGW's toolchain expects and what qld's own reader handles without a
//! special case: an `ar` archive of ordinary COFF objects whose `.idata$N`
//! sections build the import directory when they are linked.
//!
//! Three kinds of member, named so that they sort head &lt; symbols &lt; tail
//! (the grouped-section order in [`super::layout`] relies on it):
//!
//! - `<id>h.o`: the `IMAGE_IMPORT_DESCRIPTOR` in `.idata$2`, defining
//!   `_head_<id>` and referring to `__<id>_iname`;
//! - `<id>s<NNNNN>.o`, one per export: the lookup and address table entries
//!   (`.idata$4`, `.idata$5`, defining `__imp_<name>`), the hint/name in
//!   `.idata$6`, a back-reference to the head in `.idata$7`, and, for code,
//!   a `jmp *__imp_<name>` thunk in `.text` defining `<name>`;
//! - `<id>t.o`: the null terminators of the two thunk tables and the DLL
//!   name in `.idata$7`, defining `__<id>_iname`.

#![deny(clippy::arithmetic_side_effects)]

use std::path::Path;

use crate::error::{Error, Result};

use super::edata::Exports;
use super::read::consts::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_SCN_ALIGN_4BYTES, IMAGE_SCN_CNT_CODE,
    IMAGE_SCN_CNT_INITIALIZED_DATA, IMAGE_SCN_CNT_UNINITIALIZED_DATA, IMAGE_SCN_MEM_EXECUTE,
    IMAGE_SCN_MEM_READ, IMAGE_SCN_MEM_WRITE, IMAGE_SYM_CLASS_EXTERNAL, IMAGE_SYM_CLASS_STATIC,
    amd64::{IMAGE_REL_AMD64_ADDR32NB, IMAGE_REL_AMD64_REL32},
};

/// Writes the import library for `exports` to `path`.
///
/// # Errors
///
/// Returns [`Error::Io`] if the file cannot be written, and
/// [`Error::Limit`] if the library is too large for the `ar` format.
pub fn write(path: &Path, exports: &Exports, machine: u16) -> Result<()> {
    let bytes = build(path, exports, machine)?;
    std::fs::write(path, bytes).map_err(|error| Error::Io {
        path: Some(path.to_path_buf()),
        source: error,
    })
}

/// Builds the import library's bytes.
///
/// # Errors
///
/// Returns [`Error::Limit`] if a member does not fit the `ar` format.
pub fn build(path: &Path, exports: &Exports, machine: u16) -> Result<Vec<u8>> {
    let id = library_id(path);
    let head_symbol = [b"_head_".as_slice(), &id].concat();
    let iname_symbol = [b"__".as_slice(), &id, b"_iname"].concat();

    let mut members: Vec<Member> = Vec::new();
    members.push(Member {
        name: format!("{}h.o", String::from_utf8_lossy(&id)),
        data: head_member(machine, &head_symbol, &iname_symbol)?,
        defines: vec![head_symbol.clone()],
    });
    for (index, export) in exports.entries.iter().enumerate() {
        if export.private {
            continue;
        }
        let import = Import {
            symbol: export.name.clone(),
            name: if export.noname {
                ImportName::Ordinal(export.ordinal)
            } else {
                ImportName::Name {
                    hint: 0,
                    name: export.name.clone(),
                }
            },
            data: export.data,
        };
        members.push(Member {
            name: format!("{}s{index:05}.o", String::from_utf8_lossy(&id)),
            data: import_member(machine, &import, &head_symbol)?,
            defines: import.defines(),
        });
    }
    members.push(Member {
        name: format!("{}t.o", String::from_utf8_lossy(&id)),
        data: tail_member(machine, &iname_symbol, &exports.dll_name)?,
        defines: vec![iname_symbol],
    });
    archive(&members)
}

/// The identifier `dlltool` derives from the library's file name: every byte
/// that is not alphanumeric becomes `_`.
#[must_use]
pub fn library_id(path: &Path) -> Vec<u8> {
    let name = path
        .file_name()
        .map_or_else(Vec::new, |name| name.as_encoded_bytes().to_vec());
    name.iter()
        .map(|&byte| {
            if byte.is_ascii_alphanumeric() {
                byte
            } else {
                b'_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// A minimal COFF object writer
// ---------------------------------------------------------------------------

/// A section of an object under construction.
struct Section {
    name: &'static [u8],
    characteristics: u32,
    data: Vec<u8>,
    relocs: Vec<(u32, u32, u16)>,
}

/// A symbol of an object under construction.
struct Symbol {
    name: Vec<u8>,
    value: u32,
    section: i32,
    storage_class: u8,
}

/// Assembles a COFF object from sections and symbols.
fn object(machine: u16, sections: &[Section], symbols: &[Symbol]) -> Result<Vec<u8>> {
    let count = u16::try_from(sections.len())
        .map_err(|_| Error::Limit("too many sections in an import library member".into()))?;
    let header_size = 20usize.saturating_add(sections.len().saturating_mul(40));
    // Lay the contents and relocations out after the headers.
    let mut offset = header_size;
    let mut data_offset = Vec::with_capacity(sections.len());
    let mut reloc_offset = Vec::with_capacity(sections.len());
    for section in sections {
        data_offset.push(if section.data.is_empty() { 0 } else { offset });
        offset = offset.saturating_add(section.data.len());
    }
    for section in sections {
        reloc_offset.push(if section.relocs.is_empty() { 0 } else { offset });
        offset = offset.saturating_add(section.relocs.len().saturating_mul(10));
    }
    let symbol_offset = offset;

    let mut out = Vec::with_capacity(offset.saturating_add(symbols.len().saturating_mul(18)));
    out.extend_from_slice(&machine.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
    out.extend_from_slice(
        &u32::try_from(symbol_offset)
            .map_err(|_| Error::Limit("import library member too large".into()))?
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u32::try_from(symbols.len())
            .map_err(|_| Error::Limit("too many symbols".into()))?
            .to_le_bytes(),
    );
    out.extend_from_slice(&0u16.to_le_bytes()); // SizeOfOptionalHeader
    out.extend_from_slice(&0u16.to_le_bytes()); // Characteristics

    for (index, section) in sections.iter().enumerate() {
        let mut name = [0u8; 8];
        let len = section.name.len().min(8);
        if let (Some(slot), Some(source)) = (name.get_mut(..len), section.name.get(..len)) {
            slot.copy_from_slice(source);
        }
        out.extend_from_slice(&name);
        out.extend_from_slice(&0u32.to_le_bytes()); // VirtualSize
        out.extend_from_slice(&0u32.to_le_bytes()); // VirtualAddress
        out.extend_from_slice(&u32::try_from(section.data.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(data_offset.get(index).copied().unwrap_or(0))
                .unwrap_or(0)
                .to_le_bytes(),
        );
        out.extend_from_slice(
            &u32::try_from(reloc_offset.get(index).copied().unwrap_or(0))
                .unwrap_or(0)
                .to_le_bytes(),
        );
        out.extend_from_slice(&0u32.to_le_bytes()); // PointerToLinenumbers
        out.extend_from_slice(
            &u16::try_from(section.relocs.len())
                .unwrap_or(0)
                .to_le_bytes(),
        );
        out.extend_from_slice(&0u16.to_le_bytes()); // NumberOfLinenumbers
        out.extend_from_slice(&section.characteristics.to_le_bytes());
    }
    for section in sections {
        out.extend_from_slice(&section.data);
    }
    for section in sections {
        for &(address, symbol, r_type) in &section.relocs {
            out.extend_from_slice(&address.to_le_bytes());
            out.extend_from_slice(&symbol.to_le_bytes());
            out.extend_from_slice(&r_type.to_le_bytes());
        }
    }

    // Symbols, with the string table for names longer than eight bytes.
    let mut strings: Vec<u8> = vec![0, 0, 0, 0];
    for symbol in symbols {
        if symbol.name.len() <= 8 {
            let mut name = [0u8; 8];
            if let Some(slot) = name.get_mut(..symbol.name.len()) {
                slot.copy_from_slice(&symbol.name);
            }
            out.extend_from_slice(&name);
        } else {
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(
                &u32::try_from(strings.len())
                    .map_err(|_| Error::Limit("string table too large".into()))?
                    .to_le_bytes(),
            );
            strings.extend_from_slice(&symbol.name);
            strings.push(0);
        }
        out.extend_from_slice(&symbol.value.to_le_bytes());
        out.extend_from_slice(&i16::try_from(symbol.section).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // Type
        out.push(symbol.storage_class);
        out.push(0); // NumberOfAuxSymbols
    }
    let size =
        u32::try_from(strings.len()).map_err(|_| Error::Limit("string table too large".into()))?;
    if let Some(slot) = strings.first_chunk_mut::<4>() {
        *slot = size.to_le_bytes();
    }
    out.extend_from_slice(&strings);
    Ok(out)
}

/// The `.text`, `.data` and `.bss` sections every `dlltool` member starts
/// with.
fn boilerplate() -> Vec<Section> {
    let align = IMAGE_SCN_ALIGN_4BYTES;
    vec![
        Section {
            name: b".text",
            characteristics: align
                | IMAGE_SCN_CNT_CODE
                | IMAGE_SCN_MEM_EXECUTE
                | IMAGE_SCN_MEM_READ,
            data: Vec::new(),
            relocs: Vec::new(),
        },
        Section {
            name: b".data",
            characteristics: align
                | IMAGE_SCN_CNT_INITIALIZED_DATA
                | IMAGE_SCN_MEM_READ
                | IMAGE_SCN_MEM_WRITE,
            data: Vec::new(),
            relocs: Vec::new(),
        },
        Section {
            name: b".bss",
            characteristics: align
                | IMAGE_SCN_CNT_UNINITIALIZED_DATA
                | IMAGE_SCN_MEM_READ
                | IMAGE_SCN_MEM_WRITE,
            data: Vec::new(),
            relocs: Vec::new(),
        },
    ]
}

/// An `.idata$N` section's characteristics.
fn idata(name: &'static [u8], data: Vec<u8>, relocs: Vec<(u32, u32, u16)>) -> Section {
    Section {
        name,
        characteristics: IMAGE_SCN_ALIGN_4BYTES
            | IMAGE_SCN_CNT_INITIALIZED_DATA
            | IMAGE_SCN_MEM_READ
            | IMAGE_SCN_MEM_WRITE,
        data,
        relocs,
    }
}

fn external(name: &[u8], section: i32) -> Symbol {
    Symbol {
        name: name.to_vec(),
        value: 0,
        section,
        storage_class: IMAGE_SYM_CLASS_EXTERNAL,
    }
}

fn static_symbol(name: &[u8], section: i32) -> Symbol {
    Symbol {
        name: name.to_vec(),
        value: 0,
        section,
        storage_class: IMAGE_SYM_CLASS_STATIC,
    }
}

/// The head member: the import directory entry.
fn head_member(machine: u16, head_symbol: &[u8], iname_symbol: &[u8]) -> Result<Vec<u8>> {
    let mut sections = boilerplate();
    // `.idata$2` is section 4, `.idata$5` section 5, `.idata$4` section 6.
    sections.push(idata(
        b".idata$2",
        vec![0u8; 20],
        vec![
            (0, 0, IMAGE_REL_AMD64_ADDR32NB),
            (12, 3, IMAGE_REL_AMD64_ADDR32NB),
            (16, 1, IMAGE_REL_AMD64_ADDR32NB),
        ],
    ));
    sections.push(idata(b".idata$5", Vec::new(), Vec::new()));
    sections.push(idata(b".idata$4", Vec::new(), Vec::new()));
    let symbols = vec![
        static_symbol(b".idata$4", 6),
        static_symbol(b".idata$5", 5),
        external(head_symbol, 4),
        external(iname_symbol, 0),
    ];
    object(machine, &sections, &symbols)
}

/// The tail member: the null terminators and the DLL name.
fn tail_member(machine: u16, iname_symbol: &[u8], dll_name: &[u8]) -> Result<Vec<u8>> {
    let mut sections = boilerplate();
    sections.push(idata(b".idata$4", vec![0u8; 8], Vec::new()));
    sections.push(idata(b".idata$5", vec![0u8; 8], Vec::new()));
    let mut name = dll_name.to_vec();
    name.push(0);
    while !name.len().is_multiple_of(4) {
        name.push(0);
    }
    sections.push(idata(b".idata$7", name, Vec::new()));
    let symbols = vec![external(iname_symbol, 6)];
    object(machine, &sections, &symbols)
}

/// What an import refers to in the DLL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportName {
    /// Import by ordinal: no name appears in the image.
    Ordinal(u16),
    /// Import by name, with the hint the loader starts its search at.
    Name {
        /// The hint.
        hint: u16,
        /// The name the DLL exports.
        name: Vec<u8>,
    },
}

/// One import to generate a member (or a synthetic object) for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    /// The symbol the image refers to (without `__imp_`).
    pub symbol: Vec<u8>,
    /// What it refers to in the DLL.
    pub name: ImportName,
    /// Data imports define only `__imp_<symbol>`; code imports also define
    /// `<symbol>` as a jump thunk.
    pub data: bool,
}

impl Import {
    /// The symbols the member defines, for an archive symbol index or for a
    /// lazy input's name list.
    #[must_use]
    pub fn defines(&self) -> Vec<Vec<u8>> {
        let mut names = vec![[b"__imp_".as_slice(), &self.symbol].concat()];
        if !self.data {
            names.push(self.symbol.clone());
        }
        names
    }
}

/// One import: the thunk, the two table entries and the hint/name.
///
/// The sections are numbered as `dlltool` numbers them — 1 `.text`,
/// 2 `.data`, 3 `.bss`, 4 `.idata$7`, 5 `.idata$5`, 6 `.idata$4`,
/// 7 `.idata$6` — and the symbols 0 `.idata$6`, 1 `<symbol>`,
/// 2 `__imp_<symbol>`, 3 the head symbol, because the relocations refer to
/// those indices.
///
/// # Errors
///
/// Returns [`Error::Limit`] if the member does not fit the COFF format.
pub fn import_member(machine: u16, import: &Import, head_symbol: &[u8]) -> Result<Vec<u8>> {
    let symbol = &import.symbol;
    let imp = [b"__imp_".as_slice(), symbol].concat();
    let mut sections = boilerplate();
    sections.push(idata(
        b".idata$7",
        vec![0u8; 4],
        vec![(0, 3, IMAGE_REL_AMD64_ADDR32NB)],
    ));
    let (entry, entry_relocs) = match &import.name {
        ImportName::Ordinal(ordinal) => {
            // The high bit marks an ordinal import, and the ordinal is
            // stored in the entry itself.
            let mut bytes = vec![0u8; 8];
            let value = (1u64 << 63) | u64::from(*ordinal);
            if let Some(slot) = bytes.first_chunk_mut::<8>() {
                *slot = value.to_le_bytes();
            }
            (bytes, Vec::new())
        }
        ImportName::Name { .. } => (vec![0u8; 8], vec![(0u32, 0u32, IMAGE_REL_AMD64_ADDR32NB)]),
    };
    sections.push(idata(b".idata$5", entry.clone(), entry_relocs.clone()));
    sections.push(idata(b".idata$4", entry, entry_relocs));
    if let ImportName::Name { hint, name } = &import.name {
        let mut bytes = Vec::with_capacity(name.len().saturating_add(4));
        bytes.extend_from_slice(&hint.to_le_bytes());
        bytes.extend_from_slice(name);
        bytes.push(0);
        if !bytes.len().is_multiple_of(2) {
            bytes.push(0);
        }
        sections.push(idata(b".idata$6", bytes, Vec::new()));
    }
    if !import.data {
        // `jmp *__imp_<symbol>(%rip)`, relocated against the address table.
        if let Some(text) = sections.first_mut() {
            text.data = vec![0xff, 0x25, 0, 0, 0, 0, 0x90, 0x90];
            text.relocs = vec![(2, 2, IMAGE_REL_AMD64_REL32)];
        }
    }
    let idata6_section = if matches!(import.name, ImportName::Ordinal(_)) {
        0
    } else {
        7
    };
    let mut symbols = vec![static_symbol(b".idata$6", idata6_section)];
    if import.data {
        // A data import defines no thunk, so slot 1 is an unused
        // placeholder rather than an undefined external nobody satisfies.
        symbols.push(static_symbol(b".idata$5", 5));
    } else {
        symbols.push(external(symbol, 1));
    }
    symbols.push(external(&imp, 5));
    symbols.push(external(head_symbol, 0));
    object(machine, &sections, &symbols)
}

/// The head object of an import group: it defines `_head_<id>` and refers to
/// `__<id>_iname`.
///
/// # Errors
///
/// Returns [`Error::Limit`] if the object does not fit the COFF format.
pub fn head(machine: u16, head_symbol: &[u8], iname_symbol: &[u8]) -> Result<Vec<u8>> {
    head_member(machine, head_symbol, iname_symbol)
}

/// The tail object of an import group: it defines `__<id>_iname` and holds
/// the DLL name.
///
/// # Errors
///
/// Returns [`Error::Limit`] if the object does not fit the COFF format.
pub fn tail(machine: u16, iname_symbol: &[u8], dll_name: &[u8]) -> Result<Vec<u8>> {
    tail_member(machine, iname_symbol, dll_name)
}

// ---------------------------------------------------------------------------
// The `ar` archive
// ---------------------------------------------------------------------------

/// One archive member: its name, its bytes, and the symbols it defines.
struct Member {
    name: String,
    data: Vec<u8>,
    defines: Vec<Vec<u8>>,
}

/// Builds a GNU `ar` archive with a SysV symbol index.
fn archive(members: &[Member]) -> Result<Vec<u8>> {
    // Names longer than 15 bytes go in the `//` long-name member.
    let mut long_names: Vec<u8> = Vec::new();
    let mut name_field: Vec<String> = Vec::with_capacity(members.len());
    for member in members {
        let name = &member.name;
        if name.len() <= 15 {
            name_field.push(format!("{name}/"));
        } else {
            name_field.push(format!("/{}", long_names.len()));
            long_names.extend_from_slice(name.as_bytes());
            long_names.extend_from_slice(b"/\n");
        }
    }

    let symbols: Vec<(&[u8], usize)> = members
        .iter()
        .enumerate()
        .flat_map(|(index, member)| {
            member
                .defines
                .iter()
                .map(move |name| (name.as_slice(), index))
        })
        .collect();
    let index_size = 4usize
        .saturating_add(symbols.len().saturating_mul(4))
        .saturating_add(
            symbols
                .iter()
                .map(|(name, _)| name.len().saturating_add(1))
                .sum::<usize>(),
        );

    // Member offsets depend on the index size, which is now known.
    let mut offset = 8usize.saturating_add(60).saturating_add(pad(index_size));
    if !long_names.is_empty() {
        offset = offset
            .saturating_add(60)
            .saturating_add(pad(long_names.len()));
    }
    let mut offsets = Vec::with_capacity(members.len());
    for member in members {
        offsets.push(offset);
        offset = offset
            .saturating_add(60)
            .saturating_add(pad(member.data.len()));
    }

    let mut out = Vec::with_capacity(offset);
    out.extend_from_slice(b"!<arch>\n");
    let mut index = Vec::with_capacity(index_size);
    index.extend_from_slice(
        &u32::try_from(symbols.len())
            .map_err(|_| Error::Limit("too many import library symbols".into()))?
            .to_be_bytes(),
    );
    for &(_, member) in &symbols {
        let at = offsets.get(member).copied().unwrap_or(0);
        index.extend_from_slice(
            &u32::try_from(at)
                .map_err(|_| Error::Limit("import library too large".into()))?
                .to_be_bytes(),
        );
    }
    for &(name, _) in &symbols {
        index.extend_from_slice(name);
        index.push(0);
    }
    push_member(&mut out, "/", &index);
    if !long_names.is_empty() {
        push_member(&mut out, "//", &long_names);
    }
    for (member, name) in members.iter().zip(&name_field) {
        push_member(&mut out, name, &member.data);
    }
    Ok(out)
}

/// The stored size of a member's data: padded to an even length.
fn pad(size: usize) -> usize {
    size.saturating_add(size % 2)
}

/// Appends one `ar` member with a deterministic header.
fn push_member(out: &mut Vec<u8>, name: &str, data: &[u8]) {
    let mut header = [b' '; 60];
    let put = |header: &mut [u8; 60], at: usize, text: &str| {
        let end = at.saturating_add(text.len());
        if let Some(slot) = header.get_mut(at..end) {
            slot.copy_from_slice(text.as_bytes());
        }
    };
    put(&mut header, 0, name);
    // A fixed timestamp, uid, gid and mode keep the output deterministic.
    put(&mut header, 16, "0");
    put(&mut header, 28, "0");
    put(&mut header, 34, "0");
    put(&mut header, 40, "644");
    put(&mut header, 48, &data.len().to_string());
    put(&mut header, 58, "`\n");
    out.extend_from_slice(&header);
    out.extend_from_slice(data);
    if !data.len().is_multiple_of(2) {
        out.push(b'\n');
    }
}

/// Writes a module-definition file describing `exports` (`--output-def`).
///
/// # Errors
///
/// Returns [`Error::Io`] if the file cannot be written.
pub fn write_def(path: &Path, exports: &Exports) -> Result<()> {
    let mut text = Vec::new();
    text.extend_from_slice(b"EXPORTS\n");
    for export in &exports.entries {
        text.extend_from_slice(&export.name);
        text.extend_from_slice(format!(" @{}", export.ordinal).as_bytes());
        if export.noname {
            text.extend_from_slice(b" NONAME");
        }
        if export.data {
            text.extend_from_slice(b" DATA");
        }
        text.push(b'\n');
    }
    std::fs::write(path, text).map_err(|error| Error::Io {
        path: Some(path.to_path_buf()),
        source: error,
    })
}

/// The machine an import library is written for; only x86-64 is supported.
#[must_use]
pub fn supported_machine(machine: u16) -> bool {
    machine == IMAGE_FILE_MACHINE_AMD64
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::coff::edata::Export;
    use crate::coff::read::{CoffFile, Source};
    use crate::input::Archive;

    fn sample() -> Exports {
        Exports {
            entries: vec![
                Export {
                    name: b"exported_function".to_vec(),
                    symbol: Some(b"exported_function".to_vec()),
                    forwarder: None,
                    ordinal: 1,
                    noname: false,
                    data: false,
                    private: false,
                },
                Export {
                    name: b"exported_data".to_vec(),
                    symbol: Some(b"exported_data".to_vec()),
                    forwarder: None,
                    ordinal: 2,
                    noname: false,
                    data: true,
                    private: false,
                },
            ],
            dll_name: b"sample.dll".to_vec(),
            ordinal_base: 1,
        }
    }

    #[test]
    fn library_ids_follow_dlltool() {
        assert_eq!(library_id(Path::new("libfoo.dll.a")), b"libfoo_dll_a");
        assert_eq!(library_id(Path::new("/x/y/libbar.a")), b"libbar_a");
    }

    #[test]
    fn the_archive_parses_and_its_members_are_import_objects() {
        let bytes = build(
            Path::new("libsample.dll.a"),
            &sample(),
            IMAGE_FILE_MACHINE_AMD64,
        )
        .unwrap();
        let path = Path::new("libsample.dll.a");
        let archive = Archive::parse(path, &bytes).unwrap();
        let index = archive.symbol_index().expect("a symbol index");
        let names: Vec<Vec<u8>> = index
            .iter()
            .map(|symbol| symbol.unwrap().name.to_vec())
            .collect();
        assert!(
            names.contains(&b"__imp_exported_function".to_vec()),
            "{names:?}"
        );
        assert!(names.contains(&b"exported_function".to_vec()), "{names:?}");
        assert!(
            names.contains(&b"__imp_exported_data".to_vec()),
            "{names:?}"
        );
        // A data export defines no thunk.
        assert!(!names.contains(&b"exported_data".to_vec()), "{names:?}");
        assert!(
            names.contains(&b"_head_libsample_dll_a".to_vec()),
            "{names:?}"
        );
        assert!(
            names.contains(&b"__libsample_dll_a_iname".to_vec()),
            "{names:?}"
        );

        let members: Vec<_> = archive.members().map(Result::unwrap).collect();
        assert_eq!(members.len(), 4, "head, two symbols and the tail");
        for member in &members {
            let data = member.bytes().expect("a regular member");
            let file = CoffFile::parse(data, Source::new(path)).unwrap();
            assert!(matches!(file, CoffFile::Object(_)));
        }
    }

    #[test]
    fn members_sort_head_then_symbols_then_tail() {
        let bytes = build(Path::new("libs.a"), &sample(), IMAGE_FILE_MACHINE_AMD64).unwrap();
        let archive = Archive::parse(Path::new("libs.a"), &bytes).unwrap();
        let names: Vec<String> = archive
            .members()
            .map(|member| member.unwrap().display_name())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "{names:?}");
        assert!(names[0].ends_with("h.o"), "{names:?}");
        assert!(names[names.len() - 1].ends_with("t.o"), "{names:?}");
    }
}
