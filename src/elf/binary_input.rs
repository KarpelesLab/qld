//! Binary inputs (`-b binary`, `--format=binary`).
//!
//! GNU ld reads a file named after `-b binary` as a raw blob: one `.data`
//! section (allocated, writable, alignment 1) holding the file, and three
//! global symbols named after the file as it was given on the command line,
//! with every byte that is not an ASCII letter or digit turned into `_`:
//!
//! - `_binary_<name>_start` at the start of the section,
//! - `_binary_<name>_end` at its end,
//! - `_binary_<name>_size`, an absolute symbol whose value is the size.
//!
//! [`convert`] wraps the bytes in an ELF relocatable object with exactly
//! that content, in the link's class and byte order, so the rest of the
//! link treats it like any other object.

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::read::consts::{
    EM_NONE, SHF_ALLOC, SHF_WRITE, SHN_ABS, SHT_PROGBITS, SHT_STRTAB, SHT_SYMTAB, STB_GLOBAL,
    STT_NOTYPE,
};
use crate::elf::read::{ElfFormat, FileHeader, RawRecord, RawSymbol, SectionHeader};
use crate::error::{Error, Result};

/// The symbol name GNU ld derives from `path` and `suffix`: `_binary_`, the
/// path, `_`, the suffix, with every non-alphanumeric byte replaced by `_`.
#[must_use]
pub fn mangle(path: &[u8], suffix: &str) -> Vec<u8> {
    let mut name = Vec::with_capacity(path.len().saturating_add(suffix.len()).saturating_add(9));
    name.extend_from_slice(b"_binary_");
    name.extend_from_slice(path);
    name.push(b'_');
    name.extend_from_slice(suffix.as_bytes());
    for byte in &mut name {
        if !byte.is_ascii_alphanumeric() {
            *byte = b'_';
        }
    }
    name
}

fn too_large() -> Error {
    Error::Limit("binary input larger than the address space".into())
}

fn u64_of(n: usize) -> Result<u64> {
    u64::try_from(n).map_err(|_| too_large())
}

fn pad_to(out: &mut Vec<u8>, align: usize) {
    while !out.len().is_multiple_of(align.max(1)) {
        out.push(0);
    }
}

/// Wraps `data` in an ELF relocatable object as GNU ld's binary input
/// format would present it, in the link's class and byte order (`F`) and
/// machine-neutral (`EM_NONE`): it has no code and no relocations, so it
/// links into any target, and names none. `name` is the input path as
/// given on the command line.
///
/// # Errors
///
/// [`Error::Limit`] when the file or its name does not fit the format.
pub fn convert<F: ElfFormat>(name: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let size = u64_of(data.len())?;
    let symbols = [
        (mangle(name, "start"), 0u64, 1u16),
        (mangle(name, "end"), size, 1),
        (mangle(name, "size"), size, SHN_ABS),
    ];
    // String tables.
    let mut strtab = vec![0u8];
    let mut name_offsets = Vec::with_capacity(symbols.len());
    for (symbol, ..) in &symbols {
        name_offsets.push(u32::try_from(strtab.len()).map_err(|_| too_large())?);
        strtab.extend_from_slice(symbol);
        strtab.push(0);
    }
    let shstrtab_names: [&[u8]; 4] = [b".data", b".symtab", b".strtab", b".shstrtab"];
    let mut shstrtab = vec![0u8];
    let mut shname = Vec::with_capacity(shstrtab_names.len());
    for section in shstrtab_names {
        shname.push(u32::try_from(shstrtab.len()).map_err(|_| too_large())?);
        shstrtab.extend_from_slice(section);
        shstrtab.push(0);
    }

    let ehdr_size = <F::Ehdr as RawRecord>::SIZE;
    let sym_size = <F::Sym as RawRecord>::SIZE;
    let align = F::WORD_SIZE;
    let mut out = vec![0u8; ehdr_size];
    // .data
    let data_offset = out.len();
    out.extend_from_slice(data);
    pad_to(&mut out, align);
    // .symtab: null, then the three globals.
    let symtab_offset = out.len();
    out.extend_from_slice(F::encode_sym(&RawSymbol::default()).as_bytes());
    for ((_, value, shndx), name_offset) in symbols.iter().zip(&name_offsets) {
        let symbol = RawSymbol {
            st_name: *name_offset,
            st_info: (STB_GLOBAL << 4) | STT_NOTYPE,
            st_other: 0,
            st_shndx: *shndx,
            st_value: *value,
            st_size: 0,
        };
        out.extend_from_slice(F::encode_sym(&symbol).as_bytes());
    }
    let symtab_size = out.len().checked_sub(symtab_offset).ok_or_else(too_large)?;
    let strtab_offset = out.len();
    out.extend_from_slice(&strtab);
    let shstrtab_offset = out.len();
    out.extend_from_slice(&shstrtab);
    pad_to(&mut out, align);
    let shoff = out.len();

    // Section headers: null, .data, .symtab, .strtab, .shstrtab.
    let section_name = |i: usize| shname.get(i).copied().unwrap_or(0);
    let headers = [
        SectionHeader::default(),
        SectionHeader {
            sh_name: section_name(0),
            sh_type: SHT_PROGBITS,
            sh_flags: SHF_ALLOC | SHF_WRITE,
            sh_offset: u64_of(data_offset)?,
            sh_size: size,
            sh_addralign: 1,
            ..SectionHeader::default()
        },
        SectionHeader {
            sh_name: section_name(1),
            sh_type: SHT_SYMTAB,
            sh_offset: u64_of(symtab_offset)?,
            sh_size: u64_of(symtab_size)?,
            sh_link: 3,
            sh_info: 1,
            sh_addralign: u64_of(align)?,
            sh_entsize: u64_of(sym_size)?,
            ..SectionHeader::default()
        },
        SectionHeader {
            sh_name: section_name(2),
            sh_type: SHT_STRTAB,
            sh_offset: u64_of(strtab_offset)?,
            sh_size: u64_of(strtab.len())?,
            sh_addralign: 1,
            ..SectionHeader::default()
        },
        SectionHeader {
            sh_name: section_name(3),
            sh_type: SHT_STRTAB,
            sh_offset: u64_of(shstrtab_offset)?,
            sh_size: u64_of(shstrtab.len())?,
            sh_addralign: 1,
            ..SectionHeader::default()
        },
    ];
    for header in &headers {
        out.extend_from_slice(F::encode_shdr(header).as_bytes());
    }

    // ELF header.
    let file_header = FileHeader {
        ident_version: 1, // EV_CURRENT
        e_type: 1,        // ET_REL
        e_machine: EM_NONE,
        e_version: 1,
        e_shoff: u64_of(shoff)?,
        e_shnum: u16::try_from(headers.len()).map_err(|_| too_large())?,
        e_shstrndx: 4,
        ..FileHeader::default()
    };
    let encoded = F::encode_ehdr(&file_header);
    out.get_mut(..ehdr_size)
        .ok_or_else(too_large)?
        .copy_from_slice(encoded.as_bytes());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::read::{Elf64Le, ObjectFile, Source};
    use std::path::Path;

    #[test]
    fn names_are_mangled_like_gnu() {
        assert_eq!(
            mangle(b"dir/a-b.bin", "start"),
            b"_binary_dir_a_b_bin_start"
        );
        assert_eq!(mangle(b"x", "size"), b"_binary_x_size");
    }

    #[test]
    fn object_parses_with_three_symbols() {
        let bytes = convert::<Elf64Le>(b"blob.bin", b"hello").unwrap();
        let object = ObjectFile::<Elf64Le>::parse(&bytes, Source::new(Path::new("blob.bin")))
            .expect("valid object");
        let names: Vec<Vec<u8>> = object
            .symbols()
            .globals()
            .map(|s| s.unwrap().name.to_vec())
            .collect();
        assert_eq!(
            names,
            [
                b"_binary_blob_bin_start".to_vec(),
                b"_binary_blob_bin_end".to_vec(),
                b"_binary_blob_bin_size".to_vec()
            ]
        );
        let empty = convert::<Elf64Le>(b"empty", b"").unwrap();
        assert!(ObjectFile::<Elf64Le>::parse(&empty, Source::new(Path::new("empty"))).is_ok());
        // Big-endian and 32-bit links get objects of their own format.
        let be = convert::<crate::elf::read::Elf64Be>(b"blob.bin", b"hello").unwrap();
        let object =
            ObjectFile::<crate::elf::read::Elf64Be>::parse(&be, Source::new(Path::new("blob.bin")))
                .expect("valid big-endian object");
        assert_eq!(object.symbols().globals().count(), 3);
        let small = convert::<crate::elf::read::Elf32Le>(b"blob.bin", b"hello").unwrap();
        assert!(
            ObjectFile::<crate::elf::read::Elf32Le>::parse(
                &small,
                Source::new(Path::new("blob.bin"))
            )
            .is_ok()
        );
    }
}
