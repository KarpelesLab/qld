//! Notes (`SHT_NOTE`, `PT_NOTE`) and GNU program properties
//! (`.note.gnu.property`).

use core::marker::PhantomData;

use super::consts::{
    EM_386, EM_AARCH64, EM_IAMCU, EM_RISCV, EM_X86_64, GNU_PROPERTY_1_NEEDED,
    GNU_PROPERTY_AARCH64_FEATURE_1_AND, GNU_PROPERTY_NO_COPY_ON_PROTECTED,
    GNU_PROPERTY_RISCV_FEATURE_1_AND, GNU_PROPERTY_STACK_SIZE, GNU_PROPERTY_X86_FEATURE_1_AND,
    GNU_PROPERTY_X86_FEATURE_1_IBT, GNU_PROPERTY_X86_FEATURE_1_SHSTK,
    GNU_PROPERTY_X86_FEATURE_2_NEEDED, GNU_PROPERTY_X86_FEATURE_2_USED,
    GNU_PROPERTY_X86_ISA_1_NEEDED, GNU_PROPERTY_X86_ISA_1_USED,
};
use super::format::{Endian, read_u32, read_u64};
use super::source::{Source, to_u64};
use crate::error::Result;

/// One note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note<'a> {
    /// Owner name, without trailing NUL bytes (for example `b"GNU"`).
    pub name: &'a [u8],
    /// Note type; its meaning depends on the owner.
    pub n_type: u32,
    /// Descriptor bytes, without padding.
    pub desc: &'a [u8],
    /// Offset of the note within the section or segment.
    pub offset: usize,
}

/// Rounds `value` up to a multiple of `align` (a power of two).
#[inline]
fn align_up(value: usize, align: usize) -> Option<usize> {
    let mask = align.checked_sub(1)?;
    Some(value.checked_add(mask)? & !mask)
}

/// Iterator over the notes of a note section or segment.
///
/// Stops after the first malformed note, which it reports as an error.
#[derive(Debug)]
pub struct NoteIter<'a, E: Endian> {
    data: &'a [u8],
    pos: usize,
    align: usize,
    file_offset: u64,
    source: Source<'a>,
    _endian: PhantomData<E>,
}

impl<E: Endian> Clone for NoteIter<'_, E> {
    fn clone(&self) -> Self {
        Self { ..*self }
    }
}

impl<'a, E: Endian> NoteIter<'a, E> {
    /// Iterates over notes in `data`, found at `file_offset`.
    ///
    /// `sh_addralign` (or `p_align`) selects the padding: 8 for 8, and 4 for
    /// anything else, as GNU tools do.
    #[must_use]
    pub fn new(data: &'a [u8], sh_addralign: u64, file_offset: u64, source: Source<'a>) -> Self {
        let align = if sh_addralign == 8 { 8 } else { 4 };
        Self {
            data,
            pos: 0,
            align,
            file_offset,
            source,
            _endian: PhantomData,
        }
    }

    fn parse_one(&self, pos: usize) -> Option<(Note<'a>, usize)> {
        let namesz = usize::try_from(read_u32::<E>(self.data, pos)?).ok()?;
        let descsz = usize::try_from(read_u32::<E>(self.data, pos.checked_add(4)?)?).ok()?;
        let n_type = read_u32::<E>(self.data, pos.checked_add(8)?)?;
        let name_start = pos.checked_add(12)?;
        let name_end = name_start.checked_add(namesz)?;
        let mut name = self.data.get(name_start..name_end)?;
        while let Some((&0, rest)) = name.split_last() {
            name = rest;
        }
        let desc_start = align_up(name_end, self.align)?;
        let desc_end = desc_start.checked_add(descsz)?;
        let desc = self.data.get(desc_start..desc_end)?;
        // The final padding may be missing at the very end of the data.
        let next = align_up(desc_end, self.align)?.min(self.data.len());
        Some((
            Note {
                name,
                n_type,
                desc,
                offset: pos,
            },
            next,
        ))
    }
}

impl<'a, E: Endian> Iterator for NoteIter<'a, E> {
    type Item = Result<Note<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        let pos = self.pos;
        match self.parse_one(pos) {
            Some((note, next)) => {
                self.pos = next;
                Some(Ok(note))
            }
            None => {
                self.pos = self.data.len();
                Some(Err(self.source.malformed(
                    self.file_offset.saturating_add(to_u64(pos)),
                    "note",
                )))
            }
        }
    }
}

/// One GNU program property from an `NT_GNU_PROPERTY_TYPE_0` note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GnuProperty<'a> {
    /// Property type (`GNU_PROPERTY_*`).
    pub pr_type: u32,
    /// Property data, without padding.
    pub data: &'a [u8],
}

/// Iterator over the properties in an `NT_GNU_PROPERTY_TYPE_0` descriptor.
///
/// Stops after the first malformed property, which it reports as an error.
#[derive(Debug)]
pub struct GnuPropertyIter<'a, E: Endian> {
    desc: &'a [u8],
    pos: usize,
    align: usize,
    file_offset: u64,
    source: Source<'a>,
    _endian: PhantomData<E>,
}

impl<'a, E: Endian> GnuPropertyIter<'a, E> {
    /// Iterates over the properties of a note descriptor found at
    /// `file_offset`. `word_size` is the ELF class word size (4 or 8), which
    /// sets the padding.
    #[must_use]
    pub fn new(desc: &'a [u8], word_size: usize, file_offset: u64, source: Source<'a>) -> Self {
        Self {
            desc,
            pos: 0,
            align: if word_size == 8 { 8 } else { 4 },
            file_offset,
            source,
            _endian: PhantomData,
        }
    }

    fn parse_one(&self, pos: usize) -> Option<(GnuProperty<'a>, usize)> {
        let pr_type = read_u32::<E>(self.desc, pos)?;
        let size = usize::try_from(read_u32::<E>(self.desc, pos.checked_add(4)?)?).ok()?;
        let start = pos.checked_add(8)?;
        let end = start.checked_add(size)?;
        let data = self.desc.get(start..end)?;
        let next = align_up(end, self.align)?.min(self.desc.len());
        Some((GnuProperty { pr_type, data }, next))
    }
}

impl<'a, E: Endian> Iterator for GnuPropertyIter<'a, E> {
    type Item = Result<GnuProperty<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.desc.len() {
            return None;
        }
        let pos = self.pos;
        match self.parse_one(pos) {
            Some((prop, next)) => {
                self.pos = next;
                Some(Ok(prop))
            }
            None => {
                self.pos = self.desc.len();
                Some(Err(self.source.malformed(
                    self.file_offset.saturating_add(to_u64(pos)),
                    "GNU property",
                )))
            }
        }
    }
}

/// The GNU properties of one file, merged across its property notes.
///
/// `None` means the property is absent, which for "AND" feature properties
/// means the feature is not supported by the file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GnuProperties {
    /// `GNU_PROPERTY_STACK_SIZE`.
    pub stack_size: Option<u64>,
    /// `GNU_PROPERTY_NO_COPY_ON_PROTECTED` is present.
    pub no_copy_on_protected: bool,
    /// `GNU_PROPERTY_1_NEEDED` bits.
    pub needed_1: Option<u32>,
    /// x86 `GNU_PROPERTY_X86_FEATURE_1_AND` bits (IBT, SHSTK, LAM).
    pub x86_feature_1_and: Option<u32>,
    /// x86 `GNU_PROPERTY_X86_FEATURE_2_NEEDED` bits.
    pub x86_feature_2_needed: Option<u32>,
    /// x86 `GNU_PROPERTY_X86_FEATURE_2_USED` bits.
    pub x86_feature_2_used: Option<u32>,
    /// x86 `GNU_PROPERTY_X86_ISA_1_NEEDED` bits.
    pub x86_isa_1_needed: Option<u32>,
    /// x86 `GNU_PROPERTY_X86_ISA_1_USED` bits.
    pub x86_isa_1_used: Option<u32>,
    /// AArch64 `GNU_PROPERTY_AARCH64_FEATURE_1_AND` bits (BTI, PAC, GCS).
    pub aarch64_feature_1_and: Option<u32>,
    /// RISC-V `GNU_PROPERTY_RISCV_FEATURE_1_AND` bits.
    pub riscv_feature_1_and: Option<u32>,
}

impl GnuProperties {
    /// Whether the file is marked as supporting x86 indirect branch tracking.
    #[must_use]
    pub fn x86_ibt(&self) -> bool {
        self.x86_feature_1_and
            .is_some_and(|f| f & GNU_PROPERTY_X86_FEATURE_1_IBT != 0)
    }

    /// Whether the file is marked as supporting the x86 shadow stack.
    #[must_use]
    pub fn x86_shstk(&self) -> bool {
        self.x86_feature_1_and
            .is_some_and(|f| f & GNU_PROPERTY_X86_FEATURE_1_SHSTK != 0)
    }

    /// Merges the properties of one `NT_GNU_PROPERTY_TYPE_0` descriptor.
    ///
    /// `e_machine` selects how processor-specific property types are
    /// interpreted; unknown types are ignored.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the descriptor is truncated or a known
    /// property has the wrong size.
    pub fn merge_note<E: Endian>(
        &mut self,
        desc: &[u8],
        word_size: usize,
        e_machine: u16,
        file_offset: u64,
        source: Source<'_>,
    ) -> Result<()> {
        for prop in GnuPropertyIter::<E>::new(desc, word_size, file_offset, source) {
            let prop = prop?;
            let as_u32 = || {
                if prop.data.len() == 4 {
                    read_u32::<E>(prop.data, 0)
                        .ok_or_else(|| source.malformed(file_offset, "GNU property size"))
                } else {
                    Err(source.malformed(file_offset, "GNU property size"))
                }
            };
            let x86 = matches!(e_machine, EM_X86_64 | EM_386 | EM_IAMCU);
            fn and(slot: &mut Option<u32>, v: u32) {
                *slot = Some(slot.map_or(v, |old| old & v));
            }
            fn or(slot: &mut Option<u32>, v: u32) {
                *slot = Some(slot.map_or(v, |old| old | v));
            }
            match prop.pr_type {
                GNU_PROPERTY_STACK_SIZE => {
                    let size = match prop.data.len() {
                        8 => read_u64::<E>(prop.data, 0),
                        4 => read_u32::<E>(prop.data, 0).map(u64::from),
                        _ => None,
                    }
                    .ok_or_else(|| source.malformed(file_offset, "GNU property size"))?;
                    self.stack_size = Some(self.stack_size.map_or(size, |s| s.max(size)));
                }
                GNU_PROPERTY_NO_COPY_ON_PROTECTED => self.no_copy_on_protected = true,
                GNU_PROPERTY_1_NEEDED => or(&mut self.needed_1, as_u32()?),
                GNU_PROPERTY_X86_FEATURE_1_AND if x86 => {
                    and(&mut self.x86_feature_1_and, as_u32()?);
                }
                GNU_PROPERTY_X86_FEATURE_2_NEEDED if x86 => {
                    or(&mut self.x86_feature_2_needed, as_u32()?);
                }
                GNU_PROPERTY_X86_FEATURE_2_USED if x86 => {
                    or(&mut self.x86_feature_2_used, as_u32()?);
                }
                GNU_PROPERTY_X86_ISA_1_NEEDED if x86 => {
                    or(&mut self.x86_isa_1_needed, as_u32()?);
                }
                GNU_PROPERTY_X86_ISA_1_USED if x86 => {
                    or(&mut self.x86_isa_1_used, as_u32()?);
                }
                GNU_PROPERTY_AARCH64_FEATURE_1_AND if e_machine == EM_AARCH64 => {
                    and(&mut self.aarch64_feature_1_and, as_u32()?);
                }
                GNU_PROPERTY_RISCV_FEATURE_1_AND if e_machine == EM_RISCV => {
                    and(&mut self.riscv_feature_1_and, as_u32()?);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::elf::read::format::Little;
    use std::path::Path;

    fn note(name: &[u8], n_type: u32, desc: &[u8], align: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(name.len() as u32).to_le_bytes());
        v.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        v.extend_from_slice(&n_type.to_le_bytes());
        v.extend_from_slice(name);
        while v.len() % align != 0 {
            v.push(0);
        }
        v.extend_from_slice(desc);
        while v.len() % align != 0 {
            v.push(0);
        }
        v
    }

    #[test]
    fn parses_notes_and_properties() {
        let src = Source::new(Path::new("t.o"));
        let mut desc = Vec::new();
        desc.extend_from_slice(&GNU_PROPERTY_X86_FEATURE_1_AND.to_le_bytes());
        desc.extend_from_slice(&4u32.to_le_bytes());
        desc.extend_from_slice(&3u32.to_le_bytes());
        desc.extend_from_slice(&[0; 4]);
        desc.extend_from_slice(&GNU_PROPERTY_X86_ISA_1_NEEDED.to_le_bytes());
        desc.extend_from_slice(&4u32.to_le_bytes());
        desc.extend_from_slice(&1u32.to_le_bytes());
        desc.extend_from_slice(&[0; 4]);
        let mut data = note(b"GNU\0", 5, &desc, 8);
        data.extend(note(b"GNU\0", 3, &[1, 2, 3, 4, 5], 8));

        let notes: Vec<_> = NoteIter::<Little>::new(&data, 8, 0, src)
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].name, b"GNU");
        assert_eq!(notes[1].desc, &[1, 2, 3, 4, 5]);

        let mut props = GnuProperties::default();
        props
            .merge_note::<Little>(notes[0].desc, 8, EM_X86_64, 0, src)
            .unwrap();
        assert!(props.x86_ibt() && props.x86_shstk());
        assert_eq!(props.x86_isa_1_needed, Some(1));

        // A 4-aligned note (build-id style) in a 64-bit file.
        let data = note(b"GNU\0", 3, &[9; 20], 4);
        let n: Vec<_> = NoteIter::<Little>::new(&data, 4, 0, src)
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(n[0].desc, &[9; 20]);

        // Truncation reports an error once and stops.
        let mut iter = NoteIter::<Little>::new(&data[..data.len() - 3], 4, 0, src);
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());
    }
}
