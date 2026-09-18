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
//! that content, so the rest of the link treats it like any other object.

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::read::consts::{
    EM_NONE, SHF_ALLOC, SHF_WRITE, SHN_ABS, SHT_PROGBITS, SHT_STRTAB, SHT_SYMTAB, STB_GLOBAL,
    STT_NOTYPE,
};
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

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn pad_to(out: &mut Vec<u8>, align: usize) {
    while !out.len().is_multiple_of(align) {
        out.push(0);
    }
}

/// Wraps `data` in an ELF relocatable object as GNU ld's binary input
/// format would present it. The object is 64-bit little-endian and
/// machine-neutral (`EM_NONE`): it has no code and no relocations, so it
/// links into any 64-bit little-endian target, and names none. `name` is the input path as given on the
/// command line.
///
/// # Errors
///
/// [`Error::Limit`] when the file or its name does not fit the format.
#[allow(clippy::too_many_lines)]
pub fn convert(name: &[u8], data: &[u8]) -> Result<Vec<u8>> {
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

    let mut out = vec![0u8; 64];
    // .data
    let data_offset = out.len();
    out.extend_from_slice(data);
    pad_to(&mut out, 8);
    // .symtab: null, then the three globals.
    let symtab_offset = out.len();
    out.extend_from_slice(&[0u8; 24]);
    for ((_, value, shndx), name_offset) in symbols.iter().zip(&name_offsets) {
        push_u32(&mut out, *name_offset);
        out.push((STB_GLOBAL << 4) | STT_NOTYPE);
        out.push(0);
        push_u16(&mut out, *shndx);
        push_u64(&mut out, *value);
        push_u64(&mut out, 0);
    }
    let symtab_size = out.len().checked_sub(symtab_offset).ok_or_else(too_large)?;
    let strtab_offset = out.len();
    out.extend_from_slice(&strtab);
    let shstrtab_offset = out.len();
    out.extend_from_slice(&shstrtab);
    pad_to(&mut out, 8);
    let shoff = out.len();

    // Section headers: null, .data, .symtab, .strtab, .shstrtab.
    out.extend_from_slice(&[0u8; 64]);
    let header = |out: &mut Vec<u8>,
                  name: u32,
                  sh_type: u32,
                  flags: u64,
                  offset: usize,
                  size: usize,
                  link: u32,
                  info: u32,
                  align: u64,
                  entsize: u64|
     -> Result<()> {
        push_u32(out, name);
        push_u32(out, sh_type);
        push_u64(out, flags);
        push_u64(out, 0);
        push_u64(out, u64_of(offset)?);
        push_u64(out, u64_of(size)?);
        push_u32(out, link);
        push_u32(out, info);
        push_u64(out, align);
        push_u64(out, entsize);
        Ok(())
    };
    let section_name = |i: usize| shname.get(i).copied().unwrap_or(0);
    header(
        &mut out,
        section_name(0),
        SHT_PROGBITS,
        SHF_ALLOC | SHF_WRITE,
        data_offset,
        data.len(),
        0,
        0,
        1,
        0,
    )?;
    header(
        &mut out,
        section_name(1),
        SHT_SYMTAB,
        0,
        symtab_offset,
        symtab_size,
        3,
        1,
        8,
        24,
    )?;
    header(
        &mut out,
        section_name(2),
        SHT_STRTAB,
        0,
        strtab_offset,
        strtab.len(),
        0,
        0,
        1,
        0,
    )?;
    header(
        &mut out,
        section_name(3),
        SHT_STRTAB,
        0,
        shstrtab_offset,
        shstrtab.len(),
        0,
        0,
        1,
        0,
    )?;

    // ELF header.
    let ehdr = out.get_mut(..64).ok_or_else(too_large)?;
    ehdr[..4].copy_from_slice(b"\x7fELF");
    ehdr[4] = 2; // ELFCLASS64
    ehdr[5] = 1; // ELFDATA2LSB
    ehdr[6] = 1; // EV_CURRENT
    ehdr[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
    ehdr[18..20].copy_from_slice(&EM_NONE.to_le_bytes());
    ehdr[20..24].copy_from_slice(&1u32.to_le_bytes());
    ehdr[40..48].copy_from_slice(&u64_of(shoff)?.to_le_bytes());
    ehdr[52..54].copy_from_slice(&64u16.to_le_bytes());
    ehdr[58..60].copy_from_slice(&64u16.to_le_bytes());
    ehdr[60..62].copy_from_slice(&5u16.to_le_bytes());
    ehdr[62..64].copy_from_slice(&4u16.to_le_bytes());
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
        let bytes = convert(b"blob.bin", b"hello").unwrap();
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
        let empty = convert(b"empty", b"").unwrap();
        assert!(ObjectFile::<Elf64Le>::parse(&empty, Source::new(Path::new("empty"))).is_ok());
    }
}
