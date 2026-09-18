//! Symbol addresses and relocation application.
//!
//! [`Addresses`] turns a resolved symbol into a value: an RVA with its output
//! section (which `SECREL` and `SECTION` relocations need), or an absolute
//! number for `IMAGE_SYM_ABSOLUTE` symbols. It resolves, in order: symbols the
//! linker defines itself, `--alternatename` and weak-external aliases, common
//! symbols allocated in `.bss`, and ordinary definitions.
//!
//! [`apply`] writes the relocated bytes of one input section and records the
//! sites that need a base relocation, which [`super::write`] turns into
//! `.reloc`.
//!
//! COFF relocations carry their addend in the field, as ELF `SHT_REL` does.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::diag::{Diagnostic, Location};
use crate::ids::SymbolId;
use crate::symbols::{Resolution, SymbolName, SymbolTable};

use super::inputs::CoffInput;
use super::layout::Layout;
use super::machine::Machine;
use super::object::{GlobalKind, RecordTarget};
use super::read::consts::amd64::{
    IMAGE_REL_AMD64_ABSOLUTE, IMAGE_REL_AMD64_ADDR32, IMAGE_REL_AMD64_ADDR32NB,
    IMAGE_REL_AMD64_ADDR64, IMAGE_REL_AMD64_REL32, IMAGE_REL_AMD64_REL32_1,
    IMAGE_REL_AMD64_REL32_2, IMAGE_REL_AMD64_REL32_3, IMAGE_REL_AMD64_REL32_4,
    IMAGE_REL_AMD64_REL32_5, IMAGE_REL_AMD64_SECREL, IMAGE_REL_AMD64_SECREL7,
    IMAGE_REL_AMD64_SECTION,
};
use super::read::consts::arm64::IMAGE_REL_ARM64_ADDR64;
use super::read::consts::i386::{
    IMAGE_REL_I386_ABSOLUTE, IMAGE_REL_I386_DIR32, IMAGE_REL_I386_DIR32NB, IMAGE_REL_I386_REL32,
    IMAGE_REL_I386_SECREL, IMAGE_REL_I386_SECREL7, IMAGE_REL_I386_SECTION,
};
use super::read::consts::relocation_name;

/// `IMAGE_REL_BASED_HIGHLOW`: a 32-bit address to rebase.
pub const IMAGE_REL_BASED_HIGHLOW: u16 = 3;
/// `IMAGE_REL_BASED_DIR64`: a 64-bit address to rebase.
pub const IMAGE_REL_BASED_DIR64: u16 = 10;

/// What a symbol resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    /// An address in the image.
    Address {
        /// Relative virtual address.
        rva: u32,
        /// Index of the output section holding it, or `u32::MAX`.
        section: u32,
    },
    /// An absolute number that is not an address (`IMAGE_SYM_ABSOLUTE`).
    Absolute(u64),
}

impl Value {
    /// The RVA, for values that are addresses.
    #[must_use]
    pub fn rva(self) -> Option<u32> {
        match self {
            Self::Address { rva, .. } => Some(rva),
            Self::Absolute(_) => None,
        }
    }
}

/// One MinGW runtime pseudo-relocation.
///
/// The linker stores the *address of the import address table slot* in the
/// relocated field; `_pei386_runtime_relocator` replaces it at startup with
/// the slot's contents, keeping the addend. See `docs/compatibility.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PseudoReloc {
    /// RVA of the `__imp_` slot.
    pub sym: u32,
    /// RVA of the relocated field.
    pub target: u32,
    /// Width of the field in bits.
    pub flags: u32,
}

/// Version 2 header of the pseudo-relocation list: two zero words and the
/// version.
pub const PSEUDO_RELOC_V2_HEADER: [u32; 3] = [0, 0, 1];

/// Encodes the version 2 pseudo-relocation list `_pei386_runtime_relocator`
/// walks between `__RUNTIME_PSEUDO_RELOC_LIST__` and its `_END__`.
#[must_use]
pub fn encode_pseudo_relocs(relocs: &[PseudoReloc]) -> Vec<u8> {
    if relocs.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<PseudoReloc> = relocs.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = Vec::with_capacity(sorted.len().saturating_add(1).saturating_mul(12));
    for word in PSEUDO_RELOC_V2_HEADER {
        out.extend_from_slice(&word.to_le_bytes());
    }
    for reloc in &sorted {
        out.extend_from_slice(&reloc.sym.to_le_bytes());
        out.extend_from_slice(&reloc.target.to_le_bytes());
        out.extend_from_slice(&reloc.flags.to_le_bytes());
    }
    out
}

/// A site that needs an entry in `.reloc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BaseReloc {
    /// RVA of the field.
    pub rva: u32,
    /// `IMAGE_REL_BASED_*`.
    pub kind: u16,
}

/// Symbol addresses for the whole image.
#[derive(Debug)]
pub struct Addresses<'i, 'a> {
    /// The inputs.
    pub files: &'i [CoffInput<'a>],
    /// The global symbol table.
    pub symbols: &'i SymbolTable<'a>,
    /// The resolution result.
    pub resolution: &'i Resolution<'a>,
    /// The image layout.
    pub layout: &'i Layout,
    /// The image base.
    pub image_base: u64,
    /// Symbols the linker defines itself.
    pub linker: HashMap<SymbolId, Value>,
    /// Common symbols, mapped to their `.bss` address.
    pub commons: HashMap<SymbolId, Value>,
    /// `--alternatename` and weak-external fallbacks.
    pub aliases: HashMap<SymbolId, SymbolId>,
    /// Symbols MinGW auto-import binds to a DLL's import address table slot
    /// rather than to a definition, mapped to the `__imp_` symbol.
    pub auto_imported: HashMap<SymbolId, SymbolId>,
}

impl<'a> Addresses<'_, 'a> {
    /// The value of `id`, following aliases.
    #[must_use]
    pub fn value(&self, id: SymbolId) -> Option<Value> {
        let mut id = id;
        for _ in 0..16 {
            if let Some(value) = self.direct_value(id) {
                return Some(value);
            }
            match self.aliases.get(&id) {
                Some(&next) if next != id => id = next,
                _ => return None,
            }
        }
        None
    }

    /// The value of `id` without following aliases.
    fn direct_value(&self, id: SymbolId) -> Option<Value> {
        if let Some(&value) = self.linker.get(&id) {
            return Some(value);
        }
        if let Some(&value) = self.commons.get(&id) {
            return Some(value);
        }
        let definition = self.symbols.definition(id);
        if !definition.is_defined() {
            return None;
        }
        let file = definition.file.index();
        if !self.resolution.is_live(definition.file) {
            return None;
        }
        let global = self
            .files
            .get(file)?
            .object()?
            .globals
            .get(definition.index as usize)?;
        match global.kind {
            GlobalKind::Defined { section, value } => {
                let rva = self.layout.rva_of(file, section)?;
                let index = section.checked_sub(1)?;
                let out = self
                    .layout
                    .outputs
                    .get(file)
                    .and_then(|list| list.get(index as usize))
                    .copied()
                    .unwrap_or(u32::MAX);
                Some(Value::Address {
                    rva: rva.wrapping_add(value),
                    section: out,
                })
            }
            GlobalKind::Absolute(value) => Some(Value::Absolute(u64::from(value))),
            GlobalKind::Common(_) | GlobalKind::Undefined | GlobalKind::Weak { .. } => None,
        }
    }

    /// The value a relocation's symbol record refers to.
    #[must_use]
    pub fn record_value(&self, file: usize, record: u32) -> Option<Value> {
        let parsed = self.files.get(file)?.object()?;
        match parsed.targets.get(record as usize).copied()? {
            RecordTarget::None => None,
            RecordTarget::Absolute(value) => Some(Value::Absolute(u64::from(value))),
            RecordTarget::Local { section, value } => {
                let input = parsed.section(section)?;
                if input.discarded || !input.live {
                    // A reference into a discarded COMDAT copy: the kept
                    // copy's symbol is what relocations should have used.
                    return Some(Value::Absolute(0));
                }
                let rva = self.layout.rva_of(file, section)?;
                let index = section.checked_sub(1)?;
                let out = self
                    .layout
                    .outputs
                    .get(file)
                    .and_then(|list| list.get(index as usize))
                    .copied()
                    .unwrap_or(u32::MAX);
                Some(Value::Address {
                    rva: rva.wrapping_add(value),
                    section: out,
                })
            }
            RecordTarget::Global(index) => {
                let id = *self
                    .resolution
                    .symbol_ids(crate::ids::FileId::new(file))
                    .get(index as usize)?;
                self.value(id)
            }
        }
    }

    /// The symbol ID a relocation's record refers to, for diagnostics.
    #[must_use]
    pub fn record_symbol(&self, file: usize, record: u32) -> Option<SymbolId> {
        let parsed = self.files.get(file)?.object()?;
        let RecordTarget::Global(index) = parsed.targets.get(record as usize).copied()? else {
            return None;
        };
        self.resolution
            .symbol_ids(crate::ids::FileId::new(file))
            .get(index as usize)
            .copied()
    }

    /// The value of a name, if it is defined.
    #[must_use]
    pub fn by_name(&self, name: &[u8]) -> Option<Value> {
        let id = self.symbols.lookup(&SymbolName::new(name))?;
        self.value(id)
    }
}

/// The outcome of relocating one input section.
#[derive(Debug, Default)]
pub struct Applied {
    /// Sites that need a base relocation.
    pub base_relocs: Vec<BaseReloc>,
    /// Sites the MinGW runtime relocator must fix up.
    pub pseudo_relocs: Vec<PseudoReloc>,
    /// ARM64 branches that cannot reach their destination and have no
    /// range-extension thunk yet, as `(file, section, target)`.
    pub thunk_requests: Vec<(u32, u32, super::arm64::ThunkTarget)>,
    /// Problems found, as diagnostics.
    pub errors: Vec<Diagnostic>,
}

/// Writes `data` (a copy of the input section) with every relocation applied.
///
/// `rva` is the address the section was placed at. Sites that need rebasing
/// are appended to `out.base_relocs`.
pub fn apply(
    addresses: &Addresses<'_, '_>,
    file: usize,
    section: u32,
    rva: u32,
    data: &mut [u8],
    out: &mut Applied,
) {
    let Some(parsed) = addresses.files.get(file).and_then(CoffInput::object) else {
        return;
    };
    let Some(input) = parsed.section(section) else {
        return;
    };
    let machine = parsed
        .object
        .as_ref()
        .map_or(0, super::read::CoffObject::machine);
    let mut relocate = |offset: u32, r_type: u16, record: u32| {
        let auto = addresses
            .record_symbol(file, record)
            .and_then(|id| addresses.auto_imported.get(&id).copied())
            .and_then(|slot| addresses.value(slot))
            .and_then(Value::rva);
        if let Some(slot) = auto {
            let site = rva.wrapping_add(offset);
            let Some(bits) = pseudo_reloc_bits(machine, r_type) else {
                out.errors.push(
                    Diagnostic::error(format!(
                        "auto-import cannot fix up a {} relocation; declare the symbol \
                             `__declspec(dllimport)`",
                        relocation_name(machine, r_type)
                            .map_or_else(|| format!("{r_type:#x}"), str::to_string)
                    ))
                    .at(location(
                        addresses,
                        file,
                        &input.name,
                        u64::from(offset),
                    )),
                );
                return;
            };
            out.pseudo_relocs.push(PseudoReloc {
                sym: slot,
                target: site,
                flags: bits,
            });
        }
        let value = addresses.record_value(file, record);
        let Some(value) = value else {
            let name = addresses
                .record_symbol(file, record)
                .map(|id| addresses.symbols.name(id));
            out.errors.push(
                Diagnostic::error(format!(
                    "undefined symbol: {}",
                    name.map_or_else(
                        || "<unknown>".to_string(),
                        |name| crate::hints::display_symbol(name.bytes(), false).into_owned()
                    )
                ))
                .at(location(addresses, file, &input.name, u64::from(offset))),
            );
            return;
        };
        let site = Site {
            file: u32::try_from(file).unwrap_or(u32::MAX),
            section,
            offset,
            section_rva: rva,
            record,
        };
        if let Err(problem) = write_field(addresses, data, site, r_type, value, machine, out) {
            out.errors.push(Diagnostic::error(problem).at(location(
                addresses,
                file,
                &input.name,
                u64::from(offset),
            )));
        }
    };

    if input.synthetic {
        for reloc in &input.relocs {
            relocate(reloc.offset, reloc.r_type, reloc.target);
        }
        return;
    }
    let Some(object) = parsed.object.as_ref() else {
        return;
    };
    let Ok(relocations) = object.relocations(&input.header) else {
        return;
    };
    for reloc in relocations.iter() {
        relocate(
            reloc.virtual_address,
            reloc.r_type,
            reloc.symbol_table_index,
        );
    }
}

/// A diagnostic location for a relocation site.
fn location(addresses: &Addresses<'_, '_>, file: usize, section: &[u8], offset: u64) -> Location {
    let input = addresses.files.get(file);
    Location {
        file: input
            .and_then(|input| input.file)
            .map_or_else(|| "<internal>".into(), |file| file.path().to_path_buf()),
        member: input
            .and_then(|input| input.file)
            .and_then(|file| file.member())
            .map(str::to_owned),
        section: Some(String::from_utf8_lossy(section).into_owned()),
        offset: Some(offset),
        source: None,
    }
}

/// Where a relocation applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    /// Index of the input file.
    pub file: u32,
    /// The 1-based COFF section number.
    pub section: u32,
    /// Offset of the field in the section.
    pub offset: u32,
    /// RVA the section was placed at.
    pub section_rva: u32,
    /// The symbol record the relocation refers to.
    pub record: u32,
}

impl Site {
    /// RVA of the relocated field.
    #[must_use]
    pub fn rva(self) -> u32 {
        self.section_rva.wrapping_add(self.offset)
    }

    /// The field's index in the section's bytes.
    #[must_use]
    pub fn at(self) -> usize {
        self.offset as usize
    }
}

/// The width in bits of the field a runtime pseudo-relocation fixes up for
/// relocation `r_type`, or `None` when auto-import cannot handle it.
///
/// Absolute addresses and x86 PC-relative displacements work: the
/// version 2 relocator subtracts the import address table slot's address
/// from the field and adds the imported address, which is right for both.
/// ARM64 instruction fields cannot be patched that way, which is why
/// Clang reaches external data through `.refptr` pointers there.
fn pseudo_reloc_bits(machine: u16, r_type: u16) -> Option<u32> {
    match Machine::from_coff(machine).ok()? {
        Machine::Amd64 => match r_type {
            IMAGE_REL_AMD64_ADDR64 => Some(64),
            IMAGE_REL_AMD64_ADDR32
            | IMAGE_REL_AMD64_REL32
            | IMAGE_REL_AMD64_REL32_1
            | IMAGE_REL_AMD64_REL32_2
            | IMAGE_REL_AMD64_REL32_3
            | IMAGE_REL_AMD64_REL32_4
            | IMAGE_REL_AMD64_REL32_5 => Some(32),
            _ => None,
        },
        Machine::I386 => match r_type {
            IMAGE_REL_I386_DIR32 | IMAGE_REL_I386_REL32 => Some(32),
            _ => None,
        },
        Machine::Arm64 => (r_type == IMAGE_REL_ARM64_ADDR64).then_some(64),
    }
}

/// Applies one relocation to `data`.
fn write_field(
    addresses: &Addresses<'_, '_>,
    data: &mut [u8],
    site: Site,
    r_type: u16,
    value: Value,
    machine: u16,
    out: &mut Applied,
) -> Result<(), String> {
    let unknown = || {
        format!(
            "relocation type {} is not supported",
            relocation_name(machine, r_type).map_or_else(|| format!("{r_type:#x}"), str::to_string)
        )
    };
    let field = Field {
        addresses,
        site,
        value,
    };
    match Machine::from_coff(machine) {
        Ok(Machine::Amd64) => match r_type {
            IMAGE_REL_AMD64_ABSOLUTE => Ok(()),
            IMAGE_REL_AMD64_ADDR64 => field.addr64(data, out),
            IMAGE_REL_AMD64_ADDR32 => field.addr32(data, out),
            IMAGE_REL_AMD64_ADDR32NB => field.addr32nb(data),
            IMAGE_REL_AMD64_REL32
            | IMAGE_REL_AMD64_REL32_1
            | IMAGE_REL_AMD64_REL32_2
            | IMAGE_REL_AMD64_REL32_3
            | IMAGE_REL_AMD64_REL32_4
            | IMAGE_REL_AMD64_REL32_5 => {
                field.rel32(data, u32::from(r_type.wrapping_sub(IMAGE_REL_AMD64_REL32)))
            }
            IMAGE_REL_AMD64_SECREL => field.secrel(data),
            IMAGE_REL_AMD64_SECREL7 => field.secrel7(data),
            IMAGE_REL_AMD64_SECTION => field.section_index(data),
            _ => Err(unknown()),
        },
        Ok(Machine::I386) => match r_type {
            IMAGE_REL_I386_ABSOLUTE => Ok(()),
            IMAGE_REL_I386_DIR32 => field.addr32(data, out),
            IMAGE_REL_I386_DIR32NB => field.addr32nb(data),
            IMAGE_REL_I386_REL32 => field.rel32(data, 0),
            IMAGE_REL_I386_SECREL => field.secrel(data),
            IMAGE_REL_I386_SECREL7 => field.secrel7(data),
            IMAGE_REL_I386_SECTION => field.section_index(data),
            _ => Err(unknown()),
        },
        Ok(Machine::Arm64) => match super::arm64::apply(&field, data, r_type, out) {
            Some(result) => result,
            None => Err(unknown()),
        },
        Err(_) => Err(unknown()),
    }
}

/// One relocated field: where it is and the value it refers to, with the
/// computations every machine shares.
#[derive(Clone, Copy)]
pub(super) struct Field<'f, 'i, 'a> {
    /// Symbol addresses.
    pub addresses: &'f Addresses<'i, 'a>,
    /// Where the field is.
    pub site: Site,
    /// The value of the relocation's symbol.
    pub value: Value,
}

impl Field<'_, '_, '_> {
    /// The value as an RVA, with an absolute symbol read as a number.
    pub fn rva(self) -> u64 {
        match self.value {
            Value::Address { rva, .. } => u64::from(rva),
            Value::Absolute(number) => number,
        }
    }

    /// The virtual address the value names: the image base plus the RVA
    /// for an address, the number itself for an absolute symbol.
    pub fn address(self) -> u64 {
        match self.value {
            Value::Address { rva, .. } => self.addresses.image_base.wrapping_add(u64::from(rva)),
            Value::Absolute(number) => number,
        }
    }

    /// Records a base relocation of `kind` for the field, when it holds an
    /// address rather than an absolute number.
    fn rebase(self, kind: u16, out: &mut Applied) {
        if matches!(self.value, Value::Address { .. }) {
            out.base_relocs.push(BaseReloc {
                rva: self.site.rva(),
                kind,
            });
        }
    }

    /// `ADDR64`: a 64-bit virtual address.
    pub fn addr64(self, data: &mut [u8], out: &mut Applied) -> Result<(), String> {
        let slot = data
            .get_mut(self.site.at()..)
            .and_then(<[u8]>::first_chunk_mut::<8>)
            .ok_or_else(past_the_end)?;
        let addend = u64::from_le_bytes(*slot);
        *slot = self.address().wrapping_add(addend).to_le_bytes();
        self.rebase(IMAGE_REL_BASED_DIR64, out);
        Ok(())
    }

    /// `ADDR32` / i386 `DIR32`: a 32-bit virtual address, rebased with
    /// `HIGHLOW`.
    pub fn addr32(self, data: &mut [u8], out: &mut Applied) -> Result<(), String> {
        // The addend is signed: `movl table-4(,%eax,4)` stores -4.
        let addend = i64::from(read32(data, self.site.at()).cast_signed());
        let target = (self.address() as i64).wrapping_add(addend);
        let truncated = u32::try_from(target)
            .map_err(|_| format!("32-bit address relocation overflows: {target:#x}"))?;
        store32(data, self.site.at(), truncated)?;
        self.rebase(IMAGE_REL_BASED_HIGHLOW, out);
        Ok(())
    }

    /// `ADDR32NB` / i386 `DIR32NB`: a 32-bit RVA.
    pub fn addr32nb(self, data: &mut [u8]) -> Result<(), String> {
        let addend = read32(data, self.site.at());
        store32(
            data,
            self.site.at(),
            (self.rva() as u32).wrapping_add(addend),
        )
    }

    /// x86 `REL32`: a displacement from the end of the field, plus `extra`
    /// bytes of immediate that follow it (`REL32_1` … `REL32_5`).
    pub fn rel32(self, data: &mut [u8], extra: u32) -> Result<(), String> {
        let addend = i64::from(read32(data, self.site.at()).cast_signed());
        let pc = i64::from(self.site.rva())
            .wrapping_add(4)
            .wrapping_add(i64::from(extra));
        let displacement = (self.rva() as i64).wrapping_add(addend).wrapping_sub(pc);
        let truncated = i32::try_from(displacement)
            .map_err(|_| format!("PC-relative relocation out of range: {displacement:#x}"))?;
        store32(data, self.site.at(), truncated.cast_unsigned())
    }

    /// The value's offset in its output section, for `SECREL`.
    pub fn section_offset(self) -> u32 {
        section_offset(self.addresses, self.value).unwrap_or(0)
    }

    /// `SECREL`: a 32-bit offset in the output section.
    pub fn secrel(self, data: &mut [u8]) -> Result<(), String> {
        let addend = read32(data, self.site.at());
        store32(
            data,
            self.site.at(),
            self.section_offset().wrapping_add(addend),
        )
    }

    /// `SECREL7`: the low seven bits of the section offset.
    pub fn secrel7(self, data: &mut [u8]) -> Result<(), String> {
        let byte = data.get_mut(self.site.at()).ok_or_else(past_the_end)?;
        let truncated = u8::try_from(self.section_offset() & 0x7f).unwrap_or(0);
        *byte = (*byte & 0x80) | truncated;
        Ok(())
    }

    /// `SECTION`: the 1-based index of the output section, in 16 bits.
    pub fn section_index(self, data: &mut [u8]) -> Result<(), String> {
        let index = match self.value {
            Value::Address { section, .. } if section != u32::MAX => {
                u16::try_from(section.wrapping_add(1)).unwrap_or(0)
            }
            _ => 0,
        };
        let slot = data
            .get_mut(self.site.at()..)
            .and_then(<[u8]>::first_chunk_mut::<2>)
            .ok_or_else(past_the_end)?;
        *slot = index.to_le_bytes();
        Ok(())
    }
}

/// The message for a field that does not fit in its section.
pub(super) fn past_the_end() -> String {
    "relocation past the end of the section".to_string()
}

/// The little-endian word at `at`, or 0 past the end.
pub(super) fn read32(data: &[u8], at: usize) -> u32 {
    data.get(at..)
        .and_then(<[u8]>::first_chunk::<4>)
        .map_or(0, |bytes| u32::from_le_bytes(*bytes))
}

/// The offset of an address inside its output section.
fn section_offset(addresses: &Addresses<'_, '_>, value: Value) -> Option<u32> {
    let Value::Address { rva, section } = value else {
        return None;
    };
    let out = addresses.layout.sections.get(section as usize)?;
    rva.checked_sub(out.rva)
}

pub(super) fn store32(data: &mut [u8], at: usize, value: u32) -> Result<(), String> {
    let slot = data
        .get_mut(at..)
        .and_then(<[u8]>::first_chunk_mut::<4>)
        .ok_or_else(past_the_end)?;
    *slot = value.to_le_bytes();
    Ok(())
}

/// Encodes base relocations into the `.reloc` section format: per 4 KiB page,
/// a header of the page RVA and the block size, then 16-bit entries of
/// `(kind << 12) | offset`, padded to a multiple of four bytes.
#[must_use]
pub fn encode_base_relocs(sites: &[BaseReloc]) -> Vec<u8> {
    let mut sorted: Vec<BaseReloc> = sites.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = Vec::new();
    let mut index = 0usize;
    while index < sorted.len() {
        let page = sorted[index].rva & !0xfffu32;
        let start = index;
        while index < sorted.len() && sorted[index].rva & !0xfffu32 == page {
            index = index.saturating_add(1);
        }
        let entries = sorted.get(start..index).unwrap_or_default();
        let mut count = entries.len();
        // Each block's size must be a multiple of four bytes.
        let padded = count.is_multiple_of(2);
        if !padded {
            count = count.saturating_add(1);
        }
        let size = 8usize.saturating_add(count.saturating_mul(2));
        out.extend_from_slice(&page.to_le_bytes());
        out.extend_from_slice(&u32::try_from(size).unwrap_or(0).to_le_bytes());
        for entry in entries {
            let offset = u16::try_from(entry.rva & 0xfff).unwrap_or(0);
            out.extend_from_slice(&((entry.kind << 12) | offset).to_le_bytes());
        }
        if !padded {
            out.extend_from_slice(&0u16.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_relocs_group_by_page_and_pad() {
        let sites = [
            BaseReloc {
                rva: 0x1008,
                kind: IMAGE_REL_BASED_DIR64,
            },
            BaseReloc {
                rva: 0x1000,
                kind: IMAGE_REL_BASED_DIR64,
            },
            BaseReloc {
                rva: 0x2004,
                kind: IMAGE_REL_BASED_HIGHLOW,
            },
            // A duplicate must not produce a second entry.
            BaseReloc {
                rva: 0x1000,
                kind: IMAGE_REL_BASED_DIR64,
            },
        ];
        let bytes = encode_base_relocs(&sites);
        assert_eq!(&bytes[0..4], &0x1000u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &12u32.to_le_bytes());
        assert_eq!(
            u16::from_le_bytes([bytes[8], bytes[9]]),
            (IMAGE_REL_BASED_DIR64 << 12)
        );
        assert_eq!(
            u16::from_le_bytes([bytes[10], bytes[11]]),
            (IMAGE_REL_BASED_DIR64 << 12) | 8
        );
        // The second page block is padded to 12 bytes.
        assert_eq!(&bytes[12..16], &0x2000u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &12u32.to_le_bytes());
        assert_eq!(
            u16::from_le_bytes([bytes[20], bytes[21]]),
            (IMAGE_REL_BASED_HIGHLOW << 12) | 4
        );
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 0);
        assert_eq!(bytes.len(), 24);
        assert!(encode_base_relocs(&[]).is_empty());
    }
}
