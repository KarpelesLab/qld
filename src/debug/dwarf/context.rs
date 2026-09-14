//! Debug sections of a relocatable object, with their relocations applied
//! on read.
//!
//! In an `ET_REL` object, offsets into other debug sections and code
//! addresses are relocation targets: the section bytes hold zero (RELA) or
//! the addend (REL), and the relocation names a symbol. [`Context::relocate`]
//! turns a raw field into `symbol value + addend` and reports the section
//! the symbol is defined in, so a `DW_LNE_set_address` operand becomes a
//! (section index, offset) pair.

use std::borrow::Cow;

use super::reader::{DwarfError, DwarfResult};
use crate::debug::section::section_contents;
use crate::elf::read::consts::{
    EM_386, EM_AARCH64, EM_ARM, EM_LOONGARCH, EM_MIPS, EM_PPC, EM_PPC64, EM_RISCV, EM_S390,
    EM_SPARC, EM_SPARCV9, EM_X86_64,
};
use crate::elf::read::{ElfFormat, Endian, ObjectFile, Relocations, SectionIndex};
use crate::error::Result;
use crate::target::Endianness;

/// A relocation, reduced to what the DWARF readers need.
#[derive(Clone, Copy, Debug)]
struct Reloc {
    offset: u64,
    symbol: u32,
    r_type: u32,
    addend: i64,
}

/// How a relocation combines with the field it applies to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    /// `S + A` replaces the field.
    Absolute,
    /// `S + A` is added to the field (RISC-V/LoongArch `ADD*`).
    Add,
    /// `S + A` is subtracted from the field (`SUB*`).
    Sub,
    /// Not an address or offset relocation; ignored.
    Ignore,
}

fn classify(machine: u16, r_type: u32) -> Op {
    match machine {
        EM_X86_64 => match r_type {
            1 | 10 | 11 => Op::Absolute, // R_X86_64_64, _32, _32S
            _ => Op::Ignore,
        },
        EM_386 => match r_type {
            1 => Op::Absolute, // R_386_32
            _ => Op::Ignore,
        },
        EM_AARCH64 => match r_type {
            257 | 258 => Op::Absolute, // R_AARCH64_ABS64, _ABS32
            _ => Op::Ignore,
        },
        EM_ARM => match r_type {
            2 => Op::Absolute, // R_ARM_ABS32
            _ => Op::Ignore,
        },
        EM_RISCV => match r_type {
            1 | 2 | 54..=56 | 60 => Op::Absolute, // _32, _64, SET8/16/32, SET_ULEB128
            33..=36 => Op::Add,                   // ADD8/16/32/64
            37..=40 | 61 => Op::Sub,              // SUB8/16/32/64, SUB_ULEB128
            _ => Op::Ignore,
        },
        EM_LOONGARCH => match r_type {
            1 | 2 => Op::Absolute, // R_LARCH_32, _64
            47..=51 => Op::Add,    // ADD8/16/24/32/64
            52..=56 => Op::Sub,    // SUB8/16/24/32/64
            _ => Op::Ignore,
        },
        EM_PPC64 => match r_type {
            1 | 38 => Op::Absolute, // R_PPC64_ADDR32, _ADDR64
            _ => Op::Ignore,
        },
        EM_PPC => match r_type {
            1 => Op::Absolute, // R_PPC_ADDR32
            _ => Op::Ignore,
        },
        EM_S390 => match r_type {
            4 | 22 => Op::Absolute, // R_390_32, _64
            _ => Op::Ignore,
        },
        EM_MIPS => match r_type {
            2 | 18 => Op::Absolute, // R_MIPS_32, _64
            _ => Op::Ignore,
        },
        EM_SPARC | EM_SPARCV9 => match r_type {
            3 | 23 | 32 | 54 => Op::Absolute, // R_SPARC_32, _UA32, _64, _UA64
            _ => Op::Ignore,
        },
        // Unknown machine: assume the relocations in debug sections are
        // plain absolute ones.
        _ => Op::Absolute,
    }
}

/// One loaded debug section.
pub(super) struct Loaded<'a> {
    /// Contents, decompressed if needed.
    pub(super) data: Cow<'a, [u8]>,
    /// File offset of the section, for error messages.
    pub(super) file_offset: u64,
    relocs: Vec<Reloc>,
    rela: bool,
}

/// The debug sections of one object.
pub(super) struct Context<'a, F: ElfFormat> {
    object: ObjectFile<'a, F>,
    pub(super) big_endian: bool,
    machine: u16,
    /// Indexed by section index; `None` for sections not loaded.
    sections: Vec<Option<Loaded<'a>>>,
    /// Section indices by name, in index order.
    pub(super) info: Vec<u32>,
    pub(super) line: Vec<u32>,
    debug_str: Option<u32>,
    line_str: Option<u32>,
    pub(super) str_offsets: Option<u32>,
    abbrev: Option<u32>,
}

const NAMES: [&[u8]; 6] = [
    b".debug_info",
    b".debug_line",
    b".debug_str",
    b".debug_line_str",
    b".debug_str_offsets",
    b".debug_abbrev",
];

impl<'a, F: ElfFormat> Context<'a, F> {
    /// Loads (and decompresses) the debug sections of `object` that line
    /// lookup needs, with their relocations.
    pub(super) fn load(object: &ObjectFile<'a, F>) -> Result<Self> {
        let count = object.section_count();
        let mut sections: Vec<Option<Loaded<'a>>> = Vec::new();
        sections.resize_with(count, || None);
        let mut ctx = Self {
            object: *object,
            big_endian: <F::Endian as Endian>::ENDIANNESS == Endianness::Big,
            machine: object.elf().header().e_machine,
            sections,
            info: Vec::new(),
            line: Vec::new(),
            debug_str: None,
            line_str: None,
            str_offsets: None,
            abbrev: None,
        };
        let reloc_map = object.relocation_map()?;
        for (index, header) in object.elf().enumerate_sections() {
            let name = object.section_name(&header)?;
            let name = crate::debug::section::decompressed_name(name);
            let Some(which) = NAMES.iter().position(|n| **n == *name) else {
                continue;
            };
            let data = section_contents(object, &header)?;
            let mut relocs = Vec::new();
            let mut rela = true;
            let rel_index = reloc_map
                .get(usize::try_from(index).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(0);
            if rel_index != 0 {
                let rel_header = object.section_header(rel_index)?;
                if let Some(rel) = object.relocation_section(rel_index, &rel_header)? {
                    match rel.relocations {
                        Relocations::Rela(list) => {
                            relocs.extend(list.iter().map(|r| Reloc {
                                offset: r.offset,
                                symbol: r.symbol,
                                r_type: r.r_type,
                                addend: r.addend,
                            }));
                        }
                        Relocations::Rel(list) => {
                            rela = false;
                            relocs.extend(list.iter().map(|r| Reloc {
                                offset: r.offset,
                                symbol: r.symbol,
                                r_type: r.r_type,
                                addend: 0,
                            }));
                        }
                    }
                }
            }
            // Stable: relocations at the same offset keep their order.
            relocs.sort_by_key(|r| r.offset);
            if let Some(slot) = ctx
                .sections
                .get_mut(usize::try_from(index).unwrap_or(usize::MAX))
            {
                *slot = Some(Loaded {
                    data,
                    file_offset: header.sh_offset,
                    relocs,
                    rela,
                });
            }
            match which {
                0 => ctx.info.push(index),
                1 => ctx.line.push(index),
                2 => {
                    ctx.debug_str.get_or_insert(index);
                }
                3 => {
                    ctx.line_str.get_or_insert(index);
                }
                4 => {
                    ctx.str_offsets.get_or_insert(index);
                }
                _ => {
                    ctx.abbrev.get_or_insert(index);
                }
            }
        }
        Ok(ctx)
    }

    /// A loaded section.
    pub(super) fn section(&self, index: u32) -> Option<&Loaded<'a>> {
        self.sections
            .get(usize::try_from(index).ok()?)
            .and_then(Option::as_ref)
    }

    /// The `.debug_abbrev` section a unit uses: the relocation target of its
    /// abbreviation offset, else the first `.debug_abbrev`.
    pub(super) fn abbrev_section(&self, target: Option<u32>) -> Option<u32> {
        target.or(self.abbrev)
    }

    /// Applies the relocations at `offset` of section `index` to the raw
    /// field value `raw`. Returns the value and the section of the (first
    /// absolute) relocation's symbol, if any.
    pub(super) fn relocate(&self, index: u32, offset: usize, raw: u64) -> (u64, Option<u32>) {
        let Some(section) = self.section(index) else {
            return (raw, None);
        };
        let offset = u64::try_from(offset).unwrap_or(u64::MAX);
        let start = section.relocs.partition_point(|r| r.offset < offset);
        let mut value = raw;
        let mut target = None;
        for r in section.relocs.get(start..).unwrap_or_default() {
            if r.offset != offset {
                break;
            }
            let op = classify(self.machine, r.r_type);
            if op == Op::Ignore {
                continue;
            }
            let Some(sym) = self.object.symbols().get_raw(r.symbol as usize) else {
                continue;
            };
            let sym_section = match self.object.symbols().section(r.symbol as usize, &sym) {
                Ok(SectionIndex::Section(s)) => Some(s),
                _ => None,
            };
            let addend = if section.rela { r.addend as u64 } else { raw };
            let s_plus_a = sym.st_value.wrapping_add(addend);
            match op {
                Op::Absolute => {
                    value = s_plus_a;
                    if target.is_none() {
                        target = sym_section;
                    }
                }
                Op::Add => value = value.wrapping_add(s_plus_a),
                Op::Sub => value = value.wrapping_sub(s_plus_a),
                Op::Ignore => {}
            }
        }
        (value, target)
    }

    /// Reads the NUL-terminated string at `offset` in section `index`.
    pub(super) fn string_at(&self, index: u32, offset: u64) -> Option<&[u8]> {
        let data = &self.section(index)?.data;
        let rest = data.get(usize::try_from(offset).ok()?..)?;
        let len = rest.iter().position(|&b| b == 0)?;
        rest.get(..len)
    }

    /// The `.debug_str` section a `DW_FORM_strp` refers to: the relocation
    /// target if there is one, else the first `.debug_str`.
    pub(super) fn str_section(&self, target: Option<u32>) -> Option<u32> {
        target.or(self.debug_str)
    }

    /// Like [`str_section`](Self::str_section) for `.debug_line_str`.
    pub(super) fn line_str_section(&self, target: Option<u32>) -> Option<u32> {
        target.or(self.line_str)
    }

    /// Converts a DWARF error in section `index` into `Error::Malformed`.
    pub(super) fn error(&self, index: u32, error: DwarfError) -> crate::Error {
        let base = self.section(index).map_or(0, |s| s.file_offset);
        self.object.source().malformed(
            base.saturating_add(u64::try_from(error.offset).unwrap_or(u64::MAX)),
            error.what,
        )
    }
}

/// Reads a relocated unsigned field of `size` bytes at the reader's
/// position in section `index`.
pub(super) fn read_relocated<F: ElfFormat>(
    ctx: &Context<'_, F>,
    index: u32,
    reader: &mut super::reader::Reader<'_>,
    size: usize,
) -> DwarfResult<(u64, Option<u32>)> {
    let pos = reader.pos();
    let raw = reader.uint(size)?;
    Ok(ctx.relocate(index, pos, raw))
}
