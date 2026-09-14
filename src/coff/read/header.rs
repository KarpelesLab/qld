//! COFF file headers: `IMAGE_FILE_HEADER` and `ANON_OBJECT_HEADER_BIGOBJ`.

use super::consts::IMAGE_FILE_MACHINE_UNKNOWN;
use super::source::{array, u16_at, u32_at};

/// Size of `IMAGE_FILE_HEADER`.
pub const FILE_HEADER_SIZE: usize = 20;
/// Size of `ANON_OBJECT_HEADER_BIGOBJ`.
pub const BIGOBJ_HEADER_SIZE: usize = 56;
/// Size of a regular symbol record (`IMAGE_SYMBOL`).
pub const SYMBOL_SIZE: usize = 18;
/// Size of a `/bigobj` symbol record (`IMAGE_SYMBOL_EX`).
pub const BIGOBJ_SYMBOL_SIZE: usize = 20;

/// `ClassID` of `ANON_OBJECT_HEADER_BIGOBJ`,
/// `{D1BAA1C7-BAEE-4BA9-AF20-FAF66AA4DCB8}`, as stored on disk.
pub const BIGOBJ_CLASS_ID: [u8; 16] = [
    0xc7, 0xa1, 0xba, 0xd1, 0xee, 0xba, 0xa9, 0x4b, 0xaf, 0x20, 0xfa, 0xf6, 0x6a, 0xa4, 0xdc, 0xb8,
];

/// A COFF file header, normalized over the regular and `/bigobj` layouts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileHeader {
    /// `Machine` (`IMAGE_FILE_MACHINE_*`).
    pub machine: u16,
    /// `NumberOfSections` (16 bits in regular objects, 32 in `/bigobj`).
    pub number_of_sections: u32,
    /// `TimeDateStamp`.
    pub time_date_stamp: u32,
    /// `PointerToSymbolTable`: file offset of the symbol table, or 0.
    pub pointer_to_symbol_table: u32,
    /// `NumberOfSymbols`, counting auxiliary records.
    pub number_of_symbols: u32,
    /// `SizeOfOptionalHeader` (0 for objects; always 0 for `/bigobj`).
    pub size_of_optional_header: u16,
    /// `Characteristics` (`IMAGE_FILE_*`; 0 for `/bigobj`).
    pub characteristics: u16,
    /// Whether the file uses the `/bigobj` layout, with 32-bit section
    /// numbers and 20-byte symbol records.
    pub bigobj: bool,
}

impl FileHeader {
    /// Decodes a regular 20-byte `IMAGE_FILE_HEADER` at the start of `data`.
    #[must_use]
    pub fn parse_regular(data: &[u8]) -> Option<Self> {
        Some(Self {
            machine: u16_at(data, 0)?,
            number_of_sections: u32::from(u16_at(data, 2)?),
            time_date_stamp: u32_at(data, 4)?,
            pointer_to_symbol_table: u32_at(data, 8)?,
            number_of_symbols: u32_at(data, 12)?,
            size_of_optional_header: u16_at(data, 16)?,
            characteristics: u16_at(data, 18)?,
            bigobj: false,
        })
    }

    /// Decodes an `ANON_OBJECT_HEADER_BIGOBJ` at the start of `data`, or
    /// returns `None` if the signature, version or class ID does not match.
    #[must_use]
    pub fn parse_bigobj(data: &[u8]) -> Option<Self> {
        if !is_bigobj(data) {
            return None;
        }
        Some(Self {
            machine: u16_at(data, 6)?,
            time_date_stamp: u32_at(data, 8)?,
            number_of_sections: u32_at(data, 44)?,
            pointer_to_symbol_table: u32_at(data, 48)?,
            number_of_symbols: u32_at(data, 52)?,
            size_of_optional_header: 0,
            characteristics: 0,
            bigobj: true,
        })
    }

    /// Size of the header itself (20 or 56 bytes).
    #[must_use]
    pub fn header_size(&self) -> usize {
        if self.bigobj {
            BIGOBJ_HEADER_SIZE
        } else {
            FILE_HEADER_SIZE
        }
    }

    /// Size of one symbol record (18 or 20 bytes).
    #[must_use]
    pub fn symbol_size(&self) -> usize {
        if self.bigobj {
            BIGOBJ_SYMBOL_SIZE
        } else {
            SYMBOL_SIZE
        }
    }
}

/// Whether `data` starts with an `ANON_OBJECT_HEADER_BIGOBJ`:
/// `Sig1 = 0`, `Sig2 = 0xFFFF`, `Version >= 2` and the `/bigobj` class ID.
#[must_use]
pub fn is_bigobj(data: &[u8]) -> bool {
    u16_at(data, 0) == Some(IMAGE_FILE_MACHINE_UNKNOWN)
        && u16_at(data, 2) == Some(0xffff)
        && u16_at(data, 4).is_some_and(|version| version >= 2)
        && array::<16>(data, 12) == Some(BIGOBJ_CLASS_ID)
}

/// Whether `data` starts with a short import object header
/// (`Sig1 = 0`, `Sig2 = 0xFFFF`, `Version = 0`).
#[must_use]
pub fn is_import_object(data: &[u8]) -> bool {
    u16_at(data, 0) == Some(IMAGE_FILE_MACHINE_UNKNOWN)
        && u16_at(data, 2) == Some(0xffff)
        && u16_at(data, 4) == Some(0)
}
