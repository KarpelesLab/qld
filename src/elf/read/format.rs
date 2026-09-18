//! ELF class and byte-order abstraction.
//!
//! [`ElfFormat`] is implemented by four zero-sized types, [`Elf32Le`],
//! [`Elf32Be`], [`Elf64Le`] and [`Elf64Be`]. Every reader in this module is
//! generic over it, so class and endianness are resolved at compile time and
//! hot loops never branch on them.
//!
//! On-disk records are represented as byte arrays (`[u8; N]`), which have
//! alignment 1. Slices of records are obtained with `<[u8]>::as_chunks`, so
//! no pointer cast is ever made, and each record is decoded field by field
//! with `from_le_bytes` / `from_be_bytes` into a plain, host-order struct.

use core::fmt::Debug;
use core::marker::PhantomData;

use super::consts::{ELFCLASS32, ELFCLASS64, ELFDATA2LSB, ELFDATA2MSB, ELFMAG};
use super::dynamic::DynEntry;
use super::header::FileHeader;
use super::reloc::Relocation;
use super::section::{CompressionHeader, SectionHeader};
use super::segment::ProgramHeader;
use super::symbol::RawSymbol;
use crate::target::{Endianness, PointerWidth};

/// A byte order, as a type.
pub trait Endian: Copy + Default + Debug + Eq + Send + Sync + 'static {
    /// The byte order as a value.
    const ENDIANNESS: Endianness;
    /// The `e_ident[EI_DATA]` value for this byte order.
    const ELF_DATA: u8;

    /// Decodes a `u16`.
    fn u16(bytes: [u8; 2]) -> u16;
    /// Decodes a `u32`.
    fn u32(bytes: [u8; 4]) -> u32;
    /// Decodes a `u64`.
    fn u64(bytes: [u8; 8]) -> u64;

    /// Encodes a `u16`.
    fn put_u16(value: u16) -> [u8; 2];
    /// Encodes a `u32`.
    fn put_u32(value: u32) -> [u8; 4];
    /// Encodes a `u64`.
    fn put_u64(value: u64) -> [u8; 8];
}

/// Little-endian byte order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Little;

/// Big-endian byte order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Big;

impl Endian for Little {
    const ENDIANNESS: Endianness = Endianness::Little;
    const ELF_DATA: u8 = ELFDATA2LSB;

    #[inline(always)]
    fn u16(bytes: [u8; 2]) -> u16 {
        u16::from_le_bytes(bytes)
    }
    #[inline(always)]
    fn u32(bytes: [u8; 4]) -> u32 {
        u32::from_le_bytes(bytes)
    }
    #[inline(always)]
    fn u64(bytes: [u8; 8]) -> u64 {
        u64::from_le_bytes(bytes)
    }
    #[inline(always)]
    fn put_u16(value: u16) -> [u8; 2] {
        value.to_le_bytes()
    }
    #[inline(always)]
    fn put_u32(value: u32) -> [u8; 4] {
        value.to_le_bytes()
    }
    #[inline(always)]
    fn put_u64(value: u64) -> [u8; 8] {
        value.to_le_bytes()
    }
}

impl Endian for Big {
    const ENDIANNESS: Endianness = Endianness::Big;
    const ELF_DATA: u8 = ELFDATA2MSB;

    #[inline(always)]
    fn u16(bytes: [u8; 2]) -> u16 {
        u16::from_be_bytes(bytes)
    }
    #[inline(always)]
    fn u32(bytes: [u8; 4]) -> u32 {
        u32::from_be_bytes(bytes)
    }
    #[inline(always)]
    fn u64(bytes: [u8; 8]) -> u64 {
        u64::from_be_bytes(bytes)
    }
    #[inline(always)]
    fn put_u16(value: u16) -> [u8; 2] {
        value.to_be_bytes()
    }
    #[inline(always)]
    fn put_u32(value: u32) -> [u8; 4] {
        value.to_be_bytes()
    }
    #[inline(always)]
    fn put_u64(value: u64) -> [u8; 8] {
        value.to_be_bytes()
    }
}

/// A fixed-size on-disk record: a byte array with alignment 1.
pub trait RawRecord: Copy + Debug + Send + Sync + 'static {
    /// Size of the record in bytes (never zero).
    const SIZE: usize;

    /// Splits `bytes` into whole records and a trailing remainder.
    fn slice_from(bytes: &[u8]) -> (&[Self], &[u8]);

    /// Returns the raw bytes of the record.
    fn as_bytes(&self) -> &[u8];
}

impl<const N: usize> RawRecord for [u8; N] {
    const SIZE: usize = N;

    #[inline(always)]
    fn slice_from(bytes: &[u8]) -> (&[Self], &[u8]) {
        // `N` is never zero for the records used here; `as_chunks` only
        // panics for `N == 0`, which is a compile-time constant, not input.
        bytes.as_chunks::<N>()
    }

    #[inline(always)]
    fn as_bytes(&self) -> &[u8] {
        self
    }
}

/// Reads a `u16` at `offset`, or 0 if out of range.
///
/// Only used on fixed-size records whose length is known to cover `offset`,
/// where the fallback is unreachable and the bounds check folds away.
#[inline(always)]
pub(crate) fn fixed_u16<E: Endian>(bytes: &[u8], offset: usize) -> u16 {
    match bytes.get(offset..).and_then(<[u8]>::first_chunk) {
        Some(b) => E::u16(*b),
        None => 0,
    }
}

/// Reads a `u32` at `offset`, or 0 if out of range. See [`fixed_u16`].
#[inline(always)]
pub(crate) fn fixed_u32<E: Endian>(bytes: &[u8], offset: usize) -> u32 {
    match bytes.get(offset..).and_then(<[u8]>::first_chunk) {
        Some(b) => E::u32(*b),
        None => 0,
    }
}

/// Reads a `u64` at `offset`, or 0 if out of range. See [`fixed_u16`].
#[inline(always)]
pub(crate) fn fixed_u64<E: Endian>(bytes: &[u8], offset: usize) -> u64 {
    match bytes.get(offset..).and_then(<[u8]>::first_chunk) {
        Some(b) => E::u64(*b),
        None => 0,
    }
}

/// Reads a byte at `offset`, or 0 if out of range. See [`fixed_u16`].
#[inline(always)]
pub(crate) fn fixed_u8(bytes: &[u8], offset: usize) -> u8 {
    bytes.get(offset).copied().unwrap_or(0)
}

/// Stores a fixed-size field at `offset`, and does nothing if it does not
/// fit.
///
/// The records written here are of a size known at compile time, so the
/// bounds check folds away exactly as it does in [`fixed_u16`].
#[inline(always)]
fn put<const N: usize>(bytes: &mut [u8], offset: usize, value: [u8; N]) {
    if let Some(slot) = bytes
        .get_mut(offset..)
        .and_then(<[u8]>::first_chunk_mut::<N>)
    {
        *slot = value;
    }
}

/// Reads a `u16` at `offset` of variable-length data.
#[inline]
pub(crate) fn read_u16<E: Endian>(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk)
        .map(|b| E::u16(*b))
}

/// Reads a `u32` at `offset` of variable-length data.
#[inline]
pub(crate) fn read_u32<E: Endian>(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk)
        .map(|b| E::u32(*b))
}

/// Reads a `u64` at `offset` of variable-length data.
#[inline]
pub(crate) fn read_u64<E: Endian>(bytes: &[u8], offset: usize) -> Option<u64> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk)
        .map(|b| E::u64(*b))
}

/// An ELF class (32 or 64 bits) combined with a byte order.
///
/// The associated record types are byte arrays of the on-disk size, and the
/// `decode_*` functions turn one record into a host-order struct whose
/// integer fields are widened to 64 bits. All of them are infallible: the
/// length of a record is part of its type.
pub trait ElfFormat: Copy + Default + Debug + Eq + Send + Sync + 'static {
    /// The byte order.
    type Endian: Endian;

    /// `e_ident[EI_CLASS]` for this class.
    const CLASS: u8;
    /// Pointer width of the class.
    const POINTER_WIDTH: PointerWidth;
    /// Size of an address or `Elf_Word`-sized RELR entry, in bytes.
    const WORD_SIZE: usize;

    /// An `Elf_Ehdr`.
    type Ehdr: RawRecord;
    /// An `Elf_Shdr`.
    type Shdr: RawRecord;
    /// An `Elf_Phdr`.
    type Phdr: RawRecord;
    /// An `Elf_Sym`.
    type Sym: RawRecord;
    /// An `Elf_Rel`.
    type Rel: RawRecord;
    /// An `Elf_Rela`.
    type Rela: RawRecord;
    /// An `Elf_Dyn`.
    type Dyn: RawRecord;
    /// An `Elf_Chdr`.
    type Chdr: RawRecord;
    /// An address-sized word (an `Elf_Relr` entry).
    type Word: RawRecord;

    /// Decodes an ELF header.
    fn decode_ehdr(raw: &Self::Ehdr) -> FileHeader;
    /// Decodes a section header.
    fn decode_shdr(raw: &Self::Shdr) -> SectionHeader;
    /// Decodes a program header.
    fn decode_phdr(raw: &Self::Phdr) -> ProgramHeader;
    /// Decodes a symbol table entry.
    fn decode_sym(raw: &Self::Sym) -> RawSymbol;
    /// Decodes an `Elf_Rel` (the addend is reported as 0).
    fn decode_rel(raw: &Self::Rel) -> Relocation;
    /// Decodes an `Elf_Rela`.
    fn decode_rela(raw: &Self::Rela) -> Relocation;
    /// Decodes a dynamic entry.
    fn decode_dyn(raw: &Self::Dyn) -> DynEntry;
    /// Decodes a compression header.
    fn decode_chdr(raw: &Self::Chdr) -> CompressionHeader;
    /// Decodes an address-sized word.
    fn decode_word(raw: &Self::Word) -> u64;

    /// Encodes an ELF header. `e_ident` past `EI_ABIVERSION` is zeroed, and
    /// `e_ehsize`, `e_phentsize` and `e_shentsize` are taken from the class,
    /// not from `header`.
    fn encode_ehdr(header: &FileHeader) -> Self::Ehdr;
    /// Encodes a section header.
    fn encode_shdr(header: &SectionHeader) -> Self::Shdr;
    /// Encodes a program header.
    fn encode_phdr(header: &ProgramHeader) -> Self::Phdr;
    /// Encodes a symbol table entry.
    fn encode_sym(symbol: &RawSymbol) -> Self::Sym;
    /// Encodes an `Elf_Rel` (the addend is dropped).
    fn encode_rel(rel: &Relocation) -> Self::Rel;
    /// Encodes an `Elf_Rela`.
    fn encode_rela(rel: &Relocation) -> Self::Rela;
    /// Encodes a dynamic entry.
    fn encode_dyn(entry: &DynEntry) -> Self::Dyn;
    /// Encodes a compression header.
    fn encode_chdr(header: &CompressionHeader) -> Self::Chdr;
    /// Encodes an address-sized word.
    fn encode_word(value: u64) -> Self::Word;

    /// Encodes the `r_info` field of a relocation.
    fn r_info(symbol: u32, r_type: u32) -> u64;
}

/// The 64-bit ELF class with byte order `E`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Elf64<E: Endian>(PhantomData<E>);

/// The 32-bit ELF class with byte order `E`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Elf32<E: Endian>(PhantomData<E>);

/// 64-bit little-endian ELF (x86-64, AArch64, RISC-V 64, ppc64le, …).
pub type Elf64Le = Elf64<Little>;
/// 64-bit big-endian ELF (s390x, ppc64 ELFv1, …).
pub type Elf64Be = Elf64<Big>;
/// 32-bit little-endian ELF (i386, Arm, RISC-V 32, x32, …).
pub type Elf32Le = Elf32<Little>;
/// 32-bit big-endian ELF (ppc32, MIPS, …).
pub type Elf32Be = Elf32<Big>;

/// Decodes the fields `e_ident` onward common to both classes.
#[inline(always)]
fn ident_fields(b: &[u8]) -> (u8, u8, u8, u8) {
    (
        fixed_u8(b, 4),
        fixed_u8(b, 5),
        fixed_u8(b, 7),
        fixed_u8(b, 8),
    )
}

impl<E: Endian> ElfFormat for Elf64<E> {
    type Endian = E;
    const CLASS: u8 = ELFCLASS64;
    const POINTER_WIDTH: PointerWidth = PointerWidth::Bits64;
    const WORD_SIZE: usize = 8;

    type Ehdr = [u8; 64];
    type Shdr = [u8; 64];
    type Phdr = [u8; 56];
    type Sym = [u8; 24];
    type Rel = [u8; 16];
    type Rela = [u8; 24];
    type Dyn = [u8; 16];
    type Chdr = [u8; 24];
    type Word = [u8; 8];

    #[inline]
    fn decode_ehdr(b: &[u8; 64]) -> FileHeader {
        let (class, data, os_abi, abi_version) = ident_fields(b);
        FileHeader {
            class,
            data,
            ident_version: fixed_u8(b, 6),
            os_abi,
            abi_version,
            e_type: fixed_u16::<E>(b, 16),
            e_machine: fixed_u16::<E>(b, 18),
            e_version: fixed_u32::<E>(b, 20),
            e_entry: fixed_u64::<E>(b, 24),
            e_phoff: fixed_u64::<E>(b, 32),
            e_shoff: fixed_u64::<E>(b, 40),
            e_flags: fixed_u32::<E>(b, 48),
            e_ehsize: fixed_u16::<E>(b, 52),
            e_phentsize: fixed_u16::<E>(b, 54),
            e_phnum: fixed_u16::<E>(b, 56),
            e_shentsize: fixed_u16::<E>(b, 58),
            e_shnum: fixed_u16::<E>(b, 60),
            e_shstrndx: fixed_u16::<E>(b, 62),
        }
    }

    #[inline(always)]
    fn decode_shdr(b: &[u8; 64]) -> SectionHeader {
        SectionHeader {
            sh_name: fixed_u32::<E>(b, 0),
            sh_type: fixed_u32::<E>(b, 4),
            sh_flags: fixed_u64::<E>(b, 8),
            sh_addr: fixed_u64::<E>(b, 16),
            sh_offset: fixed_u64::<E>(b, 24),
            sh_size: fixed_u64::<E>(b, 32),
            sh_link: fixed_u32::<E>(b, 40),
            sh_info: fixed_u32::<E>(b, 44),
            sh_addralign: fixed_u64::<E>(b, 48),
            sh_entsize: fixed_u64::<E>(b, 56),
        }
    }

    #[inline(always)]
    fn decode_phdr(b: &[u8; 56]) -> ProgramHeader {
        ProgramHeader {
            p_type: fixed_u32::<E>(b, 0),
            p_flags: fixed_u32::<E>(b, 4),
            p_offset: fixed_u64::<E>(b, 8),
            p_vaddr: fixed_u64::<E>(b, 16),
            p_paddr: fixed_u64::<E>(b, 24),
            p_filesz: fixed_u64::<E>(b, 32),
            p_memsz: fixed_u64::<E>(b, 40),
            p_align: fixed_u64::<E>(b, 48),
        }
    }

    #[inline(always)]
    fn decode_sym(b: &[u8; 24]) -> RawSymbol {
        RawSymbol {
            st_name: fixed_u32::<E>(b, 0),
            st_info: fixed_u8(b, 4),
            st_other: fixed_u8(b, 5),
            st_shndx: fixed_u16::<E>(b, 6),
            st_value: fixed_u64::<E>(b, 8),
            st_size: fixed_u64::<E>(b, 16),
        }
    }

    #[inline(always)]
    fn decode_rel(b: &[u8; 16]) -> Relocation {
        let info = fixed_u64::<E>(b, 8);
        Relocation {
            offset: fixed_u64::<E>(b, 0),
            symbol: (info >> 32) as u32,
            r_type: info as u32,
            addend: 0,
        }
    }

    #[inline(always)]
    fn decode_rela(b: &[u8; 24]) -> Relocation {
        let info = fixed_u64::<E>(b, 8);
        Relocation {
            offset: fixed_u64::<E>(b, 0),
            symbol: (info >> 32) as u32,
            r_type: info as u32,
            addend: fixed_u64::<E>(b, 16) as i64,
        }
    }

    #[inline(always)]
    fn decode_dyn(b: &[u8; 16]) -> DynEntry {
        DynEntry {
            tag: fixed_u64::<E>(b, 0) as i64,
            value: fixed_u64::<E>(b, 8),
        }
    }

    #[inline(always)]
    fn decode_chdr(b: &[u8; 24]) -> CompressionHeader {
        CompressionHeader {
            ch_type: fixed_u32::<E>(b, 0),
            ch_size: fixed_u64::<E>(b, 8),
            ch_addralign: fixed_u64::<E>(b, 16),
        }
    }

    #[inline(always)]
    fn decode_word(b: &[u8; 8]) -> u64 {
        E::u64(*b)
    }

    fn encode_ehdr(h: &FileHeader) -> [u8; 64] {
        let mut b = [0u8; 64];
        b[..4].copy_from_slice(&ELFMAG);
        b[4] = Self::CLASS;
        b[5] = E::ELF_DATA;
        b[6] = h.ident_version;
        b[7] = h.os_abi;
        b[8] = h.abi_version;
        put(&mut b, 16, E::put_u16(h.e_type));
        put(&mut b, 18, E::put_u16(h.e_machine));
        put(&mut b, 20, E::put_u32(h.e_version));
        put(&mut b, 24, E::put_u64(h.e_entry));
        put(&mut b, 32, E::put_u64(h.e_phoff));
        put(&mut b, 40, E::put_u64(h.e_shoff));
        put(&mut b, 48, E::put_u32(h.e_flags));
        put(&mut b, 52, E::put_u16(64));
        put(&mut b, 54, E::put_u16(if h.e_phnum == 0 { 0 } else { 56 }));
        put(&mut b, 56, E::put_u16(h.e_phnum));
        put(&mut b, 58, E::put_u16(64));
        put(&mut b, 60, E::put_u16(h.e_shnum));
        put(&mut b, 62, E::put_u16(h.e_shstrndx));
        b
    }

    #[inline(always)]
    fn encode_shdr(h: &SectionHeader) -> [u8; 64] {
        let mut b = [0u8; 64];
        put(&mut b, 0, E::put_u32(h.sh_name));
        put(&mut b, 4, E::put_u32(h.sh_type));
        put(&mut b, 8, E::put_u64(h.sh_flags));
        put(&mut b, 16, E::put_u64(h.sh_addr));
        put(&mut b, 24, E::put_u64(h.sh_offset));
        put(&mut b, 32, E::put_u64(h.sh_size));
        put(&mut b, 40, E::put_u32(h.sh_link));
        put(&mut b, 44, E::put_u32(h.sh_info));
        put(&mut b, 48, E::put_u64(h.sh_addralign));
        put(&mut b, 56, E::put_u64(h.sh_entsize));
        b
    }

    #[inline(always)]
    fn encode_phdr(h: &ProgramHeader) -> [u8; 56] {
        let mut b = [0u8; 56];
        put(&mut b, 0, E::put_u32(h.p_type));
        put(&mut b, 4, E::put_u32(h.p_flags));
        put(&mut b, 8, E::put_u64(h.p_offset));
        put(&mut b, 16, E::put_u64(h.p_vaddr));
        put(&mut b, 24, E::put_u64(h.p_paddr));
        put(&mut b, 32, E::put_u64(h.p_filesz));
        put(&mut b, 40, E::put_u64(h.p_memsz));
        put(&mut b, 48, E::put_u64(h.p_align));
        b
    }

    #[inline(always)]
    fn encode_sym(s: &RawSymbol) -> [u8; 24] {
        let mut b = [0u8; 24];
        put(&mut b, 0, E::put_u32(s.st_name));
        b[4] = s.st_info;
        b[5] = s.st_other;
        put(&mut b, 6, E::put_u16(s.st_shndx));
        put(&mut b, 8, E::put_u64(s.st_value));
        put(&mut b, 16, E::put_u64(s.st_size));
        b
    }

    #[inline(always)]
    fn encode_rel(r: &Relocation) -> [u8; 16] {
        let mut b = [0u8; 16];
        put(&mut b, 0, E::put_u64(r.offset));
        put(&mut b, 8, E::put_u64(Self::r_info(r.symbol, r.r_type)));
        b
    }

    #[inline(always)]
    fn encode_rela(r: &Relocation) -> [u8; 24] {
        let mut b = [0u8; 24];
        put(&mut b, 0, E::put_u64(r.offset));
        put(&mut b, 8, E::put_u64(Self::r_info(r.symbol, r.r_type)));
        put(&mut b, 16, E::put_u64(r.addend as u64));
        b
    }

    #[inline(always)]
    fn encode_dyn(d: &DynEntry) -> [u8; 16] {
        let mut b = [0u8; 16];
        put(&mut b, 0, E::put_u64(d.tag as u64));
        put(&mut b, 8, E::put_u64(d.value));
        b
    }

    #[inline(always)]
    fn encode_chdr(h: &CompressionHeader) -> [u8; 24] {
        let mut b = [0u8; 24];
        put(&mut b, 0, E::put_u32(h.ch_type));
        put(&mut b, 8, E::put_u64(h.ch_size));
        put(&mut b, 16, E::put_u64(h.ch_addralign));
        b
    }

    #[inline(always)]
    fn encode_word(value: u64) -> [u8; 8] {
        E::put_u64(value)
    }

    #[inline(always)]
    fn r_info(symbol: u32, r_type: u32) -> u64 {
        (u64::from(symbol) << 32) | u64::from(r_type)
    }
}

impl<E: Endian> ElfFormat for Elf32<E> {
    type Endian = E;
    const CLASS: u8 = ELFCLASS32;
    const POINTER_WIDTH: PointerWidth = PointerWidth::Bits32;
    const WORD_SIZE: usize = 4;

    type Ehdr = [u8; 52];
    type Shdr = [u8; 40];
    type Phdr = [u8; 32];
    type Sym = [u8; 16];
    type Rel = [u8; 8];
    type Rela = [u8; 12];
    type Dyn = [u8; 8];
    type Chdr = [u8; 12];
    type Word = [u8; 4];

    #[inline]
    fn decode_ehdr(b: &[u8; 52]) -> FileHeader {
        let (class, data, os_abi, abi_version) = ident_fields(b);
        FileHeader {
            class,
            data,
            ident_version: fixed_u8(b, 6),
            os_abi,
            abi_version,
            e_type: fixed_u16::<E>(b, 16),
            e_machine: fixed_u16::<E>(b, 18),
            e_version: fixed_u32::<E>(b, 20),
            e_entry: u64::from(fixed_u32::<E>(b, 24)),
            e_phoff: u64::from(fixed_u32::<E>(b, 28)),
            e_shoff: u64::from(fixed_u32::<E>(b, 32)),
            e_flags: fixed_u32::<E>(b, 36),
            e_ehsize: fixed_u16::<E>(b, 40),
            e_phentsize: fixed_u16::<E>(b, 42),
            e_phnum: fixed_u16::<E>(b, 44),
            e_shentsize: fixed_u16::<E>(b, 46),
            e_shnum: fixed_u16::<E>(b, 48),
            e_shstrndx: fixed_u16::<E>(b, 50),
        }
    }

    #[inline(always)]
    fn decode_shdr(b: &[u8; 40]) -> SectionHeader {
        SectionHeader {
            sh_name: fixed_u32::<E>(b, 0),
            sh_type: fixed_u32::<E>(b, 4),
            sh_flags: u64::from(fixed_u32::<E>(b, 8)),
            sh_addr: u64::from(fixed_u32::<E>(b, 12)),
            sh_offset: u64::from(fixed_u32::<E>(b, 16)),
            sh_size: u64::from(fixed_u32::<E>(b, 20)),
            sh_link: fixed_u32::<E>(b, 24),
            sh_info: fixed_u32::<E>(b, 28),
            sh_addralign: u64::from(fixed_u32::<E>(b, 32)),
            sh_entsize: u64::from(fixed_u32::<E>(b, 36)),
        }
    }

    #[inline(always)]
    fn decode_phdr(b: &[u8; 32]) -> ProgramHeader {
        ProgramHeader {
            p_type: fixed_u32::<E>(b, 0),
            p_offset: u64::from(fixed_u32::<E>(b, 4)),
            p_vaddr: u64::from(fixed_u32::<E>(b, 8)),
            p_paddr: u64::from(fixed_u32::<E>(b, 12)),
            p_filesz: u64::from(fixed_u32::<E>(b, 16)),
            p_memsz: u64::from(fixed_u32::<E>(b, 20)),
            p_flags: fixed_u32::<E>(b, 24),
            p_align: u64::from(fixed_u32::<E>(b, 28)),
        }
    }

    #[inline(always)]
    fn decode_sym(b: &[u8; 16]) -> RawSymbol {
        RawSymbol {
            st_name: fixed_u32::<E>(b, 0),
            st_value: u64::from(fixed_u32::<E>(b, 4)),
            st_size: u64::from(fixed_u32::<E>(b, 8)),
            st_info: fixed_u8(b, 12),
            st_other: fixed_u8(b, 13),
            st_shndx: fixed_u16::<E>(b, 14),
        }
    }

    #[inline(always)]
    fn decode_rel(b: &[u8; 8]) -> Relocation {
        let info = fixed_u32::<E>(b, 4);
        Relocation {
            offset: u64::from(fixed_u32::<E>(b, 0)),
            symbol: info >> 8,
            r_type: info & 0xff,
            addend: 0,
        }
    }

    #[inline(always)]
    fn decode_rela(b: &[u8; 12]) -> Relocation {
        let info = fixed_u32::<E>(b, 4);
        Relocation {
            offset: u64::from(fixed_u32::<E>(b, 0)),
            symbol: info >> 8,
            r_type: info & 0xff,
            addend: i64::from(fixed_u32::<E>(b, 8) as i32),
        }
    }

    #[inline(always)]
    fn decode_dyn(b: &[u8; 8]) -> DynEntry {
        DynEntry {
            tag: i64::from(fixed_u32::<E>(b, 0) as i32),
            value: u64::from(fixed_u32::<E>(b, 4)),
        }
    }

    #[inline(always)]
    fn decode_chdr(b: &[u8; 12]) -> CompressionHeader {
        CompressionHeader {
            ch_type: fixed_u32::<E>(b, 0),
            ch_size: u64::from(fixed_u32::<E>(b, 4)),
            ch_addralign: u64::from(fixed_u32::<E>(b, 8)),
        }
    }

    #[inline(always)]
    fn decode_word(b: &[u8; 4]) -> u64 {
        u64::from(E::u32(*b))
    }

    fn encode_ehdr(h: &FileHeader) -> [u8; 52] {
        let mut b = [0u8; 52];
        b[..4].copy_from_slice(&ELFMAG);
        b[4] = Self::CLASS;
        b[5] = E::ELF_DATA;
        b[6] = h.ident_version;
        b[7] = h.os_abi;
        b[8] = h.abi_version;
        put(&mut b, 16, E::put_u16(h.e_type));
        put(&mut b, 18, E::put_u16(h.e_machine));
        put(&mut b, 20, E::put_u32(h.e_version));
        put(&mut b, 24, E::put_u32(h.e_entry as u32));
        put(&mut b, 28, E::put_u32(h.e_phoff as u32));
        put(&mut b, 32, E::put_u32(h.e_shoff as u32));
        put(&mut b, 36, E::put_u32(h.e_flags));
        put(&mut b, 40, E::put_u16(52));
        put(&mut b, 42, E::put_u16(if h.e_phnum == 0 { 0 } else { 32 }));
        put(&mut b, 44, E::put_u16(h.e_phnum));
        put(&mut b, 46, E::put_u16(40));
        put(&mut b, 48, E::put_u16(h.e_shnum));
        put(&mut b, 50, E::put_u16(h.e_shstrndx));
        b
    }

    #[inline(always)]
    fn encode_shdr(h: &SectionHeader) -> [u8; 40] {
        let mut b = [0u8; 40];
        put(&mut b, 0, E::put_u32(h.sh_name));
        put(&mut b, 4, E::put_u32(h.sh_type));
        put(&mut b, 8, E::put_u32(h.sh_flags as u32));
        put(&mut b, 12, E::put_u32(h.sh_addr as u32));
        put(&mut b, 16, E::put_u32(h.sh_offset as u32));
        put(&mut b, 20, E::put_u32(h.sh_size as u32));
        put(&mut b, 24, E::put_u32(h.sh_link));
        put(&mut b, 28, E::put_u32(h.sh_info));
        put(&mut b, 32, E::put_u32(h.sh_addralign as u32));
        put(&mut b, 36, E::put_u32(h.sh_entsize as u32));
        b
    }

    #[inline(always)]
    fn encode_phdr(h: &ProgramHeader) -> [u8; 32] {
        let mut b = [0u8; 32];
        put(&mut b, 0, E::put_u32(h.p_type));
        put(&mut b, 4, E::put_u32(h.p_offset as u32));
        put(&mut b, 8, E::put_u32(h.p_vaddr as u32));
        put(&mut b, 12, E::put_u32(h.p_paddr as u32));
        put(&mut b, 16, E::put_u32(h.p_filesz as u32));
        put(&mut b, 20, E::put_u32(h.p_memsz as u32));
        put(&mut b, 24, E::put_u32(h.p_flags));
        put(&mut b, 28, E::put_u32(h.p_align as u32));
        b
    }

    #[inline(always)]
    fn encode_sym(s: &RawSymbol) -> [u8; 16] {
        let mut b = [0u8; 16];
        put(&mut b, 0, E::put_u32(s.st_name));
        put(&mut b, 4, E::put_u32(s.st_value as u32));
        put(&mut b, 8, E::put_u32(s.st_size as u32));
        b[12] = s.st_info;
        b[13] = s.st_other;
        put(&mut b, 14, E::put_u16(s.st_shndx));
        b
    }

    #[inline(always)]
    fn encode_rel(r: &Relocation) -> [u8; 8] {
        let mut b = [0u8; 8];
        put(&mut b, 0, E::put_u32(r.offset as u32));
        put(
            &mut b,
            4,
            E::put_u32(Self::r_info(r.symbol, r.r_type) as u32),
        );
        b
    }

    #[inline(always)]
    fn encode_rela(r: &Relocation) -> [u8; 12] {
        let mut b = [0u8; 12];
        put(&mut b, 0, E::put_u32(r.offset as u32));
        put(
            &mut b,
            4,
            E::put_u32(Self::r_info(r.symbol, r.r_type) as u32),
        );
        put(&mut b, 8, E::put_u32(r.addend as u32));
        b
    }

    #[inline(always)]
    fn encode_dyn(d: &DynEntry) -> [u8; 8] {
        let mut b = [0u8; 8];
        put(&mut b, 0, E::put_u32(d.tag as u32));
        put(&mut b, 4, E::put_u32(d.value as u32));
        b
    }

    #[inline(always)]
    fn encode_chdr(h: &CompressionHeader) -> [u8; 12] {
        let mut b = [0u8; 12];
        put(&mut b, 0, E::put_u32(h.ch_type));
        put(&mut b, 4, E::put_u32(h.ch_size as u32));
        put(&mut b, 8, E::put_u32(h.ch_addralign as u32));
        b
    }

    #[inline(always)]
    fn encode_word(value: u64) -> [u8; 4] {
        E::put_u32(value as u32)
    }

    #[inline(always)]
    fn r_info(symbol: u32, r_type: u32) -> u64 {
        u64::from(symbol.wrapping_shl(8) | (r_type & 0xff))
    }
}

/// One of the four ELF class/byte-order combinations, known at run time.
///
/// Use [`ElfKind::identify`] once per file, then dispatch to the generic
/// readers with the matching [`ElfFormat`] type. The default is
/// [`Elf64Le`], the shape of every architecture qld linked before ELF32
/// existed here; anything that writes an output sets it from
/// [`Arch::kind`](crate::elf::arch::Arch::kind).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ElfKind {
    /// [`Elf32Le`].
    Elf32Le,
    /// [`Elf32Be`].
    Elf32Be,
    /// [`Elf64Le`].
    #[default]
    Elf64Le,
    /// [`Elf64Be`].
    Elf64Be,
}

impl ElfKind {
    /// Identifies the class and byte order from `e_ident`.
    ///
    /// Returns `None` when `data` does not start with the ELF magic or has an
    /// unknown class or data encoding. Nothing else is validated.
    #[must_use]
    pub fn identify(data: &[u8]) -> Option<Self> {
        let ident = data.first_chunk::<6>()?;
        if ident[..4] != super::consts::ELFMAG {
            return None;
        }
        match (ident[4], ident[5]) {
            (ELFCLASS32, ELFDATA2LSB) => Some(Self::Elf32Le),
            (ELFCLASS32, ELFDATA2MSB) => Some(Self::Elf32Be),
            (ELFCLASS64, ELFDATA2LSB) => Some(Self::Elf64Le),
            (ELFCLASS64, ELFDATA2MSB) => Some(Self::Elf64Be),
            _ => None,
        }
    }

    /// The byte order.
    #[must_use]
    pub fn endianness(self) -> Endianness {
        match self {
            Self::Elf32Le | Self::Elf64Le => Endianness::Little,
            Self::Elf32Be | Self::Elf64Be => Endianness::Big,
        }
    }

    /// The pointer width of the class.
    #[must_use]
    pub fn pointer_width(self) -> PointerWidth {
        match self {
            Self::Elf32Le | Self::Elf32Be => PointerWidth::Bits32,
            Self::Elf64Le | Self::Elf64Be => PointerWidth::Bits64,
        }
    }

    /// Whether the class is 32-bit.
    #[must_use]
    #[inline]
    pub fn is_32(self) -> bool {
        matches!(self, Self::Elf32Le | Self::Elf32Be)
    }

    /// Size of an address, and of one `.got` or `.relr.dyn` entry.
    #[must_use]
    #[inline]
    pub fn word_size(self) -> u64 {
        if self.is_32() { 4 } else { 8 }
    }

    /// Size of the ELF header.
    #[must_use]
    #[inline]
    pub fn ehdr_size(self) -> u64 {
        if self.is_32() { 52 } else { 64 }
    }

    /// Size of one program header.
    #[must_use]
    #[inline]
    pub fn phdr_size(self) -> u64 {
        if self.is_32() { 32 } else { 56 }
    }

    /// Size of one section header.
    #[must_use]
    #[inline]
    pub fn shdr_size(self) -> u64 {
        if self.is_32() { 40 } else { 64 }
    }

    /// Size of one symbol table entry.
    #[must_use]
    #[inline]
    pub fn sym_size(self) -> u64 {
        if self.is_32() { 16 } else { 24 }
    }

    /// Size of one `Elf_Rel` entry.
    #[must_use]
    #[inline]
    pub fn rel_size(self) -> u64 {
        if self.is_32() { 8 } else { 16 }
    }

    /// Size of one `Elf_Rela` entry.
    #[must_use]
    #[inline]
    pub fn rela_size(self) -> u64 {
        if self.is_32() { 12 } else { 24 }
    }

    /// Size of one `.dynamic` entry.
    #[must_use]
    #[inline]
    pub fn dyn_size(self) -> u64 {
        if self.is_32() { 8 } else { 16 }
    }
}

/// Runs `$body` with `$f` bound to the [`ElfFormat`] type of `$kind`.
///
/// The choice is made once, so the code inside is monomorphized and never
/// branches on class or byte order.
macro_rules! with_format {
    ($kind:expr, |$f:ident| $body:block) => {{
        use $crate::elf::read::{Elf32Be, Elf32Le, Elf64Be, Elf64Le, ElfKind};
        match $kind {
            ElfKind::Elf64Le => {
                type $f = Elf64Le;
                $body
            }
            ElfKind::Elf32Le => {
                type $f = Elf32Le;
                $body
            }
            ElfKind::Elf64Be => {
                type $f = Elf64Be;
                $body
            }
            ElfKind::Elf32Be => {
                type $f = Elf32Be;
                $body
            }
        }
    }};
}
pub(crate) use with_format;

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn record_sizes() {
        assert_eq!(<Elf64Le as ElfFormat>::Ehdr::SIZE, 64);
        assert_eq!(<Elf64Be as ElfFormat>::Sym::SIZE, 24);
        assert_eq!(<Elf32Le as ElfFormat>::Shdr::SIZE, 40);
        assert_eq!(<Elf32Be as ElfFormat>::Rela::SIZE, 12);
    }

    #[test]
    fn decodes_rela_in_both_orders() {
        let mut le = [0u8; 24];
        le[..8].copy_from_slice(&0x1122u64.to_le_bytes());
        le[8..16].copy_from_slice(&((7u64 << 32) | 2).to_le_bytes());
        le[16..].copy_from_slice(&(-4i64).to_le_bytes());
        let r = Elf64Le::decode_rela(&le);
        assert_eq!((r.offset, r.symbol, r.r_type, r.addend), (0x1122, 7, 2, -4));

        let mut be = [0u8; 12];
        be[..4].copy_from_slice(&0x10u32.to_be_bytes());
        be[4..8].copy_from_slice(&((5u32 << 8) | 9).to_be_bytes());
        be[8..].copy_from_slice(&(-8i32).to_be_bytes());
        let r = Elf32Be::decode_rela(&be);
        assert_eq!((r.offset, r.symbol, r.r_type, r.addend), (0x10, 5, 9, -8));
    }

    /// Every record encodes and decodes back to the same value, in both
    /// classes and both byte orders.
    fn round_trip<F: ElfFormat>(mask: u64) {
        let header = FileHeader {
            class: F::CLASS,
            data: <F::Endian as Endian>::ELF_DATA,
            ident_version: 1,
            os_abi: 3,
            abi_version: 0,
            e_type: 3,
            e_machine: 62,
            e_version: 1,
            e_entry: 0x1234_5678 & mask,
            e_phoff: 0x40,
            e_shoff: 0x9abc_def0 & mask,
            e_flags: 0x55,
            e_ehsize: u16::try_from(<F::Ehdr as RawRecord>::SIZE).unwrap(),
            e_phentsize: u16::try_from(<F::Phdr as RawRecord>::SIZE).unwrap(),
            e_phnum: 7,
            e_shentsize: u16::try_from(<F::Shdr as RawRecord>::SIZE).unwrap(),
            e_shnum: 11,
            e_shstrndx: 10,
        };
        assert_eq!(F::decode_ehdr(&F::encode_ehdr(&header)), header);

        let shdr = SectionHeader {
            sh_name: 3,
            sh_type: 1,
            sh_flags: 0x6 & mask,
            sh_addr: 0x1000 & mask,
            sh_offset: 0x2000,
            sh_size: 0x3000,
            sh_link: 4,
            sh_info: 5,
            sh_addralign: 16,
            sh_entsize: 24,
        };
        assert_eq!(F::decode_shdr(&F::encode_shdr(&shdr)), shdr);

        let phdr = ProgramHeader {
            p_type: 1,
            p_flags: 5,
            p_offset: 0x1000,
            p_vaddr: 0x40_1000 & mask,
            p_paddr: 0x40_1000 & mask,
            p_filesz: 0x123,
            p_memsz: 0x456,
            p_align: 0x1000,
        };
        assert_eq!(F::decode_phdr(&F::encode_phdr(&phdr)), phdr);

        let sym = RawSymbol {
            st_name: 9,
            st_info: 0x12,
            st_other: 2,
            st_shndx: 6,
            st_value: 0xdead_beef & mask,
            st_size: 0x40,
        };
        assert_eq!(F::decode_sym(&F::encode_sym(&sym)), sym);

        // ELF32 has 24 bits of symbol index and 8 of type.
        let (symbol, r_type) = if F::WORD_SIZE == 8 {
            (0x0012_3456, 42)
        } else {
            (0x1234, 42)
        };
        let rela = Relocation {
            offset: 0x2468 & mask,
            symbol,
            r_type,
            addend: -12,
        };
        assert_eq!(F::decode_rela(&F::encode_rela(&rela)), rela);
        let rel = Relocation { addend: 0, ..rela };
        assert_eq!(F::decode_rel(&F::encode_rel(&rela)), rel);

        let entry = DynEntry {
            tag: 30,
            value: 0x7fff_0000 & mask,
        };
        assert_eq!(F::decode_dyn(&F::encode_dyn(&entry)), entry);

        let chdr = CompressionHeader {
            ch_type: 1,
            ch_size: 0x1_2345 & mask,
            ch_addralign: 8,
        };
        assert_eq!(F::decode_chdr(&F::encode_chdr(&chdr)), chdr);

        let word = 0x0123_4567_89ab_cdef & mask;
        assert_eq!(F::decode_word(&F::encode_word(word)), word);
    }

    #[test]
    fn records_round_trip() {
        round_trip::<Elf64Le>(u64::MAX);
        round_trip::<Elf64Be>(u64::MAX);
        round_trip::<Elf32Le>(u64::from(u32::MAX));
        round_trip::<Elf32Be>(u64::from(u32::MAX));
    }

    /// The encoders write the bytes the ABI puts at each offset.
    #[test]
    fn encodes_at_the_abi_offsets() {
        let sym = RawSymbol {
            st_name: 1,
            st_info: 0x10,
            st_other: 0,
            st_shndx: 2,
            st_value: 0x3040,
            st_size: 8,
        };
        // Elf32_Sym reorders the fields: name, value, size, info, other,
        // shndx.
        let raw = Elf32Be::encode_sym(&sym);
        assert_eq!(
            raw,
            [0, 0, 0, 1, 0, 0, 0x30, 0x40, 0, 0, 0, 8, 0x10, 0, 0, 2]
        );

        // Elf32_Phdr puts p_flags after p_memsz, not after p_type.
        let phdr = ProgramHeader {
            p_type: 1,
            p_flags: 4,
            p_offset: 0,
            p_vaddr: 0,
            p_paddr: 0,
            p_filesz: 0,
            p_memsz: 0,
            p_align: 1,
        };
        let raw = Elf32Le::encode_phdr(&phdr);
        assert_eq!(&raw[24..28], &4u32.to_le_bytes());
    }

    #[test]
    fn identifies_kinds() {
        assert_eq!(
            ElfKind::identify(b"\x7fELF\x02\x01"),
            Some(ElfKind::Elf64Le)
        );
        assert_eq!(
            ElfKind::identify(b"\x7fELF\x01\x02"),
            Some(ElfKind::Elf32Be)
        );
        assert_eq!(ElfKind::identify(b"\x7fELF\x03\x01"), None);
        assert_eq!(ElfKind::identify(b"\x7fEL"), None);
        assert_eq!(ElfKind::identify(b"!<arch>\n"), None);
    }
}
