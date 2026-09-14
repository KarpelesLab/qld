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

use super::consts::{ELFCLASS32, ELFCLASS64, ELFDATA2LSB, ELFDATA2MSB};
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
}

/// One of the four ELF class/byte-order combinations, known at run time.
///
/// Use [`ElfKind::identify`] once per file, then dispatch to the generic
/// readers with the matching [`ElfFormat`] type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ElfKind {
    /// [`Elf32Le`].
    Elf32Le,
    /// [`Elf32Be`].
    Elf32Be,
    /// [`Elf64Le`].
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
}

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
