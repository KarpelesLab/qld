//! `__TEXT,__eh_frame` record splitting.
//!
//! The section is a sequence of CIE and FDE records, as in ELF `.eh_frame`
//! (the approach mirrors `elf::read::eh_frame`, without depending on it).
//! Mach-O differs in two ways that matter here:
//!
//! - Assemblers resolve references to local functions themselves, so an
//!   FDE's `pc_begin` is often a plain PC-relative value with no
//!   relocation. This module decodes the pointer encodings from the CIE
//!   augmentation and computes the address each FDE describes, so the
//!   linker can find the function by address when there is no relocation.
//! - References that do need relocating (personality pointers through the
//!   GOT, `pc_begin` of global functions) use `SUBTRACTOR`/`UNSIGNED` pairs
//!   or `*_POINTER_TO_GOT`/`X86_64_RELOC_GOT` entries; they are returned per
//!   record, and per field where a field has one.
//!
//! Call frame instructions are not interpreted.

use core::ops::Range;

use super::bytes::{Endian, Source, cstr, read_sleb, read_uleb, to_u64};
use super::object::ObjectFile;
use super::reloc::PairedRelocation;
use crate::error::Result;

/// `DW_EH_PE_omit`.
pub const DW_EH_PE_OMIT: u8 = 0xff;
/// `DW_EH_PE_absptr`.
pub const DW_EH_PE_ABSPTR: u8 = 0x00;
/// `DW_EH_PE_pcrel`.
pub const DW_EH_PE_PCREL: u8 = 0x10;
/// `DW_EH_PE_indirect`.
pub const DW_EH_PE_INDIRECT: u8 = 0x80;

/// A pointer field of a CIE or FDE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EhPointer {
    /// `DW_EH_PE_*` encoding.
    pub encoding: u8,
    /// Offset of the field within the section.
    pub offset: usize,
    /// Size of the field in bytes.
    pub size: usize,
    /// The value stored in the section, sign- or zero-extended.
    pub value: u64,
    /// The address the value denotes: for PC-relative encodings, the section
    /// address plus the field offset plus the value. When
    /// `DW_EH_PE_indirect` is set, this is the address of the pointer to
    /// the target.
    pub address: u64,
    /// The relocation starting at this field, if any.
    pub relocation: Option<PairedRelocation>,
}

/// A decoded CIE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cie<'a> {
    /// Version (1 or 3).
    pub version: u8,
    /// Augmentation string, such as `zPLR`.
    pub augmentation: &'a [u8],
    /// Code alignment factor.
    pub code_alignment: u64,
    /// Data alignment factor.
    pub data_alignment: i64,
    /// Return address register.
    pub return_register: u64,
    /// The personality routine pointer (`P`).
    pub personality: Option<EhPointer>,
    /// The LSDA pointer encoding in FDEs (`L`).
    pub lsda_encoding: Option<u8>,
    /// The `pc_begin` encoding in FDEs (`R`; absolute pointers otherwise).
    pub fde_encoding: u8,
    /// Signal frame (`S`).
    pub signal_frame: bool,
}

/// A decoded FDE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fde {
    /// Section offset of the CIE.
    pub cie_offset: usize,
    /// Index of the CIE in [`EhFrame::records`].
    pub cie_index: usize,
    /// The start of the function.
    pub pc_begin: EhPointer,
    /// The length of the function.
    pub pc_range: u64,
    /// The LSDA pointer, when the CIE has `L` and the value is not omitted.
    pub lsda: Option<EhPointer>,
}

/// What an `__eh_frame` record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EhFrameKind<'a> {
    /// A common information entry.
    Cie(Cie<'a>),
    /// A frame description entry.
    Fde(Fde),
}

/// One CIE or FDE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhFrameRecord<'a> {
    /// Offset of the record within the section.
    pub offset: usize,
    /// The whole record, including its length field.
    pub data: &'a [u8],
    /// Size of the length field: 4, or 12 for the 64-bit DWARF form.
    pub length_size: usize,
    /// CIE or FDE, decoded.
    pub kind: EhFrameKind<'a>,
    /// Indices into [`EhFrame::relocations`] of the relocations inside the
    /// record.
    pub relocations: Range<usize>,
}

/// The records of an `__eh_frame` section.
#[derive(Clone, Debug, Default)]
pub struct EhFrame<'a> {
    /// The records, in section order.
    pub records: Vec<EhFrameRecord<'a>>,
    /// The section's relocations, sorted by address.
    pub relocations: Vec<PairedRelocation>,
}

/// Parameters for [`EhFrame::parse`].
#[derive(Clone, Copy, Debug)]
pub struct EhFrameInput<'a> {
    /// Section contents.
    pub data: &'a [u8],
    /// Section address in the object.
    pub address: u64,
    /// Pointer size in bytes (8 or 4).
    pub word_size: usize,
    /// Byte order.
    pub endian: Endian,
    /// File offset of the section, for errors.
    pub file_offset: u64,
    /// Error context.
    pub source: Source<'a>,
}

struct Reader<'d> {
    data: &'d [u8],
    pos: usize,
    endian: Endian,
}

impl Reader<'_> {
    fn u8(&mut self) -> Option<u8> {
        let value = *self.data.get(self.pos)?;
        self.pos = self.pos.checked_add(1)?;
        Some(value)
    }
    fn uleb(&mut self) -> Option<u64> {
        read_uleb(self.data, &mut self.pos)
    }
    fn sleb(&mut self) -> Option<i64> {
        read_sleb(self.data, &mut self.pos)
    }
    /// Reads a value in `encoding` (low nibble), returning (size, value).
    fn encoded(&mut self, encoding: u8, word_size: usize) -> Option<(usize, u64)> {
        let start = self.pos;
        let e = self.endian;
        let (size, value) = match encoding & 0x0f {
            0x00 => (word_size, e.word(self.data, start, word_size == 8)?),
            0x01 => {
                let v = self.uleb()?;
                return Some((self.pos.checked_sub(start)?, v));
            }
            0x09 => {
                let v = self.sleb()?;
                return Some((self.pos.checked_sub(start)?, v as u64));
            }
            0x02 => (2, u64::from(e.u16(self.data, start)?)),
            0x03 => (4, u64::from(e.u32(self.data, start)?)),
            0x04 | 0x0c => (8, e.u64(self.data, start)?),
            0x0a => (2, i64::from(e.u16(self.data, start)? as i16) as u64),
            0x0b => (4, i64::from(e.u32(self.data, start)? as i32) as u64),
            _ => return None,
        };
        // Sign-extend 4-byte absolute pointers on 32-bit targets is not
        // needed: the value is an address.
        self.pos = start.checked_add(size)?;
        Some((size, value))
    }
}

impl<'a> EhFrame<'a> {
    /// Splits and decodes the records of `input.data`.
    ///
    /// `relocations` are the section's relocations in any order.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` for a truncated or inconsistent record, an
    /// FDE whose CIE pointer does not name an earlier CIE, an unsupported
    /// pointer encoding, or a relocation outside every record.
    pub fn parse(input: EhFrameInput<'a>, mut relocations: Vec<PairedRelocation>) -> Result<Self> {
        relocations.sort_by_key(|r| (r.relocation.address, r.first_index));
        let data = input.data;
        let mut records: Vec<EhFrameRecord<'a>> = Vec::new();
        let mut pos = 0usize;
        let mut next_reloc = 0usize;
        while pos < data.len() {
            let fail = |at: usize, what: &str| {
                input.source.malformed(
                    input.file_offset.saturating_add(to_u64(at)),
                    format!("__eh_frame record ({what})"),
                )
            };
            let length = input
                .endian
                .u32(data, pos)
                .ok_or_else(|| fail(pos, "truncated length"))?;
            if length == 0 {
                // Terminator.
                break;
            }
            let (length_size, length) = if length == u32::MAX {
                let ext = pos
                    .checked_add(4)
                    .and_then(|at| input.endian.u64(data, at))
                    .ok_or_else(|| fail(pos, "truncated length"))?;
                (12usize, ext)
            } else {
                (4usize, u64::from(length))
            };
            let end = usize::try_from(length)
                .ok()
                .and_then(|l| pos.checked_add(length_size)?.checked_add(l))
                .filter(|&end| end <= data.len())
                .ok_or_else(|| fail(pos, "extends past end of section"))?;
            if length < 4 {
                return Err(fail(pos, "too short"));
            }
            let record = data.get(pos..end).unwrap_or(&[]);
            let id_pos = pos.saturating_add(length_size);
            let id = input.endian.u32(data, id_pos).unwrap_or(0);

            // Relocations inside the record.
            let first_reloc = next_reloc;
            while let Some(r) = relocations.get(next_reloc) {
                let address = usize::try_from(r.relocation.address).unwrap_or(usize::MAX);
                if address >= end {
                    break;
                }
                if address < pos {
                    return Err(fail(address, "relocation outside any record"));
                }
                next_reloc = next_reloc.saturating_add(1);
            }
            let record_relocs = relocations.get(first_reloc..next_reloc).unwrap_or(&[]);
            let reloc_at = |offset: usize| {
                record_relocs
                    .iter()
                    .find(|r| usize::try_from(r.relocation.address).ok() == Some(offset))
                    .copied()
            };
            let mut reader = Reader {
                data: record,
                pos: id_pos.saturating_add(4).saturating_sub(pos),
                endian: input.endian,
            };
            let pointer = |reader: &mut Reader<'_>, encoding: u8| -> Option<EhPointer> {
                let field = reader.pos;
                let (size, value) = reader.encoded(encoding, input.word_size)?;
                let offset = pos.checked_add(field)?;
                let address = if encoding & 0x70 == DW_EH_PE_PCREL {
                    input
                        .address
                        .wrapping_add(to_u64(offset))
                        .wrapping_add(value)
                } else {
                    value
                };
                Some(EhPointer {
                    encoding,
                    offset,
                    size,
                    value,
                    address,
                    relocation: reloc_at(offset),
                })
            };

            let kind = if id == 0 {
                let mut parse_cie = || -> Option<Cie<'a>> {
                    let version = reader.u8()?;
                    let augmentation = cstr(record.get(reader.pos..)?)?;
                    reader.pos = reader.pos.checked_add(augmentation.len())?.checked_add(1)?;
                    if augmentation.windows(2).any(|w| w == b"eh") {
                        reader.pos = reader.pos.checked_add(input.word_size)?;
                    }
                    let code_alignment = reader.uleb()?;
                    let data_alignment = reader.sleb()?;
                    let return_register = if version == 1 {
                        u64::from(reader.u8()?)
                    } else {
                        reader.uleb()?
                    };
                    let mut cie = Cie {
                        version,
                        augmentation,
                        code_alignment,
                        data_alignment,
                        return_register,
                        personality: None,
                        lsda_encoding: None,
                        fde_encoding: DW_EH_PE_ABSPTR,
                        signal_frame: false,
                    };
                    if augmentation.first() == Some(&b'z') {
                        let _length = reader.uleb()?;
                        for &c in augmentation.get(1..)? {
                            match c {
                                b'L' => cie.lsda_encoding = Some(reader.u8()?),
                                b'P' => {
                                    let encoding = reader.u8()?;
                                    cie.personality = Some(pointer(&mut reader, encoding)?);
                                }
                                b'R' => cie.fde_encoding = reader.u8()?,
                                b'S' => cie.signal_frame = true,
                                b'B' | b'G' => {}
                                _ => return None,
                            }
                        }
                    }
                    Some(cie)
                };
                EhFrameKind::Cie(parse_cie().ok_or_else(|| fail(pos, "malformed CIE"))?)
            } else {
                let cie_offset = usize::try_from(id)
                    .ok()
                    .and_then(|id| id_pos.checked_sub(id))
                    .ok_or_else(|| fail(pos, "CIE pointer out of range"))?;
                let cie_index = records
                    .binary_search_by_key(&cie_offset, |r| r.offset)
                    .ok()
                    .ok_or_else(|| fail(pos, "CIE pointer does not name a record"))?;
                let EhFrameKind::Cie(cie) = records
                    .get(cie_index)
                    .map(|r| r.kind)
                    .ok_or_else(|| fail(pos, "CIE pointer does not name a record"))?
                else {
                    return Err(fail(pos, "CIE pointer names an FDE"));
                };
                let mut parse_fde = || -> Option<Fde> {
                    let pc_begin = pointer(&mut reader, cie.fde_encoding)?;
                    let (_, pc_range) = reader.encoded(cie.fde_encoding & 0x0f, input.word_size)?;
                    let mut lsda = None;
                    if cie.augmentation.first() == Some(&b'z') {
                        let _length = reader.uleb()?;
                        if let Some(encoding) = cie.lsda_encoding
                            && encoding != DW_EH_PE_OMIT
                        {
                            lsda = Some(pointer(&mut reader, encoding)?);
                        }
                    }
                    Some(Fde {
                        cie_offset,
                        cie_index,
                        pc_begin,
                        pc_range,
                        lsda,
                    })
                };
                EhFrameKind::Fde(parse_fde().ok_or_else(|| fail(pos, "malformed FDE"))?)
            };
            records.push(EhFrameRecord {
                offset: pos,
                data: record,
                length_size,
                kind,
                relocations: first_reloc..next_reloc,
            });
            pos = end;
        }
        if let Some(r) = relocations.get(next_reloc) {
            return Err(input.source.malformed(
                input
                    .file_offset
                    .saturating_add(u64::from(r.relocation.address)),
                "__eh_frame relocation (outside any record)",
            ));
        }
        Ok(Self {
            records,
            relocations,
        })
    }

    /// Decodes the `__TEXT,__eh_frame` section of `object`, if it has one.
    ///
    /// # Errors
    ///
    /// See [`parse`](Self::parse).
    pub fn from_object(object: &ObjectFile<'a>) -> Result<Option<Self>> {
        let Some((index, section)) = object.find_section(b"__TEXT", b"__eh_frame") else {
            return Ok(None);
        };
        let relocations = object
            .paired_relocations(index)?
            .collect::<Result<Vec<_>>>()?;
        let input = EhFrameInput {
            data: object.section_data(index)?,
            address: section.addr,
            word_size: if object.file().is64() { 8 } else { 4 },
            endian: object.file().endian(),
            file_offset: u64::from(section.offset),
            source: object.source(),
        };
        Self::parse(input, relocations).map(Some)
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use std::path::Path;

    fn record(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        v.extend_from_slice(&id.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn decodes_records() {
        let src = Source::new(Path::new("t.o"));
        // CIE: version 1, "zPLR", code 1, data -8, ra 30, aug len,
        // P = pcrel|indirect|sdata4 (0x9b) + 4 bytes, L = pcrel (0x10), R = pcrel (0x10).
        let mut body = vec![1];
        body.extend_from_slice(b"zPLR\0");
        body.extend_from_slice(&[1, 0x78, 30, 7, 0x9b, 0xf0, 0xff, 0xff, 0xff, 0x10, 0x10]);
        body.resize(24, 0); // DW_CFA_nop padding
        let mut data = record(0, &body);
        let fde_at = data.len();
        // FDE: pc_begin (8, pcrel), pc_range (8), aug len 8, lsda (8, pcrel).
        let mut fde = Vec::new();
        fde.extend_from_slice(&(-0x100i64).to_le_bytes());
        fde.extend_from_slice(&0x40u64.to_le_bytes());
        fde.push(8);
        fde.extend_from_slice(&0x20u64.to_le_bytes());
        data.extend(record((fde_at + 4) as u32, &fde));
        let input = EhFrameInput {
            data: &data,
            address: 0x1000,
            word_size: 8,
            endian: Endian::LITTLE,
            file_offset: 0,
            source: src,
        };
        let frame = EhFrame::parse(input, Vec::new()).unwrap();
        assert_eq!(frame.records.len(), 2);
        let EhFrameKind::Cie(cie) = frame.records[0].kind else {
            panic!()
        };
        assert_eq!(cie.augmentation, b"zPLR");
        assert_eq!(cie.data_alignment, -8);
        let personality = cie.personality.unwrap();
        assert_eq!(personality.offset, 19);
        assert_eq!(personality.address, 0x1000 + 19 - 16);
        let EhFrameKind::Fde(fde) = frame.records[1].kind else {
            panic!()
        };
        assert_eq!(fde.cie_index, 0);
        assert_eq!(fde.pc_begin.address, 0x1000 + (fde_at as u64 + 8) - 0x100);
        assert_eq!(fde.pc_range, 0x40);
        assert_eq!(
            fde.lsda.unwrap().address,
            0x1000 + fde_at as u64 + 25 + 0x20
        );

        // An FDE pointing at nothing.
        let bad = record(1000, &[0; 20]);
        let input = EhFrameInput {
            data: &bad,
            ..input
        };
        assert!(EhFrame::parse(input, Vec::new()).is_err());
    }
}
