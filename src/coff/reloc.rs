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
use super::object::{GlobalKind, RecordTarget};
use super::read::consts::amd64::{
    IMAGE_REL_AMD64_ABSOLUTE, IMAGE_REL_AMD64_ADDR32, IMAGE_REL_AMD64_ADDR32NB,
    IMAGE_REL_AMD64_ADDR64, IMAGE_REL_AMD64_REL32, IMAGE_REL_AMD64_REL32_1,
    IMAGE_REL_AMD64_REL32_2, IMAGE_REL_AMD64_REL32_3, IMAGE_REL_AMD64_REL32_4,
    IMAGE_REL_AMD64_REL32_5, IMAGE_REL_AMD64_SECREL, IMAGE_REL_AMD64_SECREL7,
    IMAGE_REL_AMD64_SECTION,
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
        if let Err(problem) = write_field(addresses, data, offset, rva, r_type, value, machine, out)
        {
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

/// Applies one relocation to `data`.
#[allow(clippy::too_many_arguments)]
fn write_field(
    addresses: &Addresses<'_, '_>,
    data: &mut [u8],
    offset: u32,
    section_rva: u32,
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
    let site = section_rva.wrapping_add(offset);
    let at = offset as usize;
    let read32 = |data: &[u8]| -> u32 {
        data.get(at..)
            .and_then(<[u8]>::first_chunk::<4>)
            .map_or(0, |bytes| u32::from_le_bytes(*bytes))
    };
    match r_type {
        IMAGE_REL_AMD64_ABSOLUTE => Ok(()),
        IMAGE_REL_AMD64_ADDR64 => {
            let slot = data
                .get_mut(at..)
                .and_then(<[u8]>::first_chunk_mut::<8>)
                .ok_or_else(|| "relocation past the end of the section".to_string())?;
            let addend = u64::from_le_bytes(*slot);
            let target = match value {
                Value::Address { rva, .. } => addresses
                    .image_base
                    .wrapping_add(u64::from(rva))
                    .wrapping_add(addend),
                Value::Absolute(number) => number.wrapping_add(addend),
            };
            *slot = target.to_le_bytes();
            if matches!(value, Value::Address { .. }) {
                out.base_relocs.push(BaseReloc {
                    rva: site,
                    kind: IMAGE_REL_BASED_DIR64,
                });
            }
            Ok(())
        }
        IMAGE_REL_AMD64_ADDR32 => {
            let addend = read32(data);
            let target = match value {
                Value::Address { rva, .. } => addresses
                    .image_base
                    .wrapping_add(u64::from(rva))
                    .wrapping_add(u64::from(addend)),
                Value::Absolute(number) => number.wrapping_add(u64::from(addend)),
            };
            let truncated = u32::try_from(target)
                .map_err(|_| format!("32-bit address relocation overflows: {target:#x}"))?;
            store32(data, at, truncated)?;
            if matches!(value, Value::Address { .. }) {
                out.base_relocs.push(BaseReloc {
                    rva: site,
                    kind: IMAGE_REL_BASED_HIGHLOW,
                });
            }
            Ok(())
        }
        IMAGE_REL_AMD64_ADDR32NB => {
            let addend = read32(data);
            let target = match value {
                Value::Address { rva, .. } => rva.wrapping_add(addend),
                Value::Absolute(number) => (number as u32).wrapping_add(addend),
            };
            store32(data, at, target)
        }
        IMAGE_REL_AMD64_REL32
        | IMAGE_REL_AMD64_REL32_1
        | IMAGE_REL_AMD64_REL32_2
        | IMAGE_REL_AMD64_REL32_3
        | IMAGE_REL_AMD64_REL32_4
        | IMAGE_REL_AMD64_REL32_5 => {
            let extra = i64::from(r_type.wrapping_sub(IMAGE_REL_AMD64_REL32));
            let addend = i64::from(read32(data).cast_signed());
            let target = match value {
                Value::Address { rva, .. } => i64::from(rva),
                Value::Absolute(number) => number as i64,
            };
            let pc = i64::from(site).wrapping_add(4).wrapping_add(extra);
            let displacement = target.wrapping_add(addend).wrapping_sub(pc);
            let truncated = i32::try_from(displacement)
                .map_err(|_| format!("PC-relative relocation out of range: {displacement:#x}"))?;
            store32(data, at, truncated.cast_unsigned())
        }
        IMAGE_REL_AMD64_SECREL => {
            let addend = read32(data);
            let offset_in_section = section_offset(addresses, value).unwrap_or(0);
            store32(data, at, offset_in_section.wrapping_add(addend))
        }
        IMAGE_REL_AMD64_SECREL7 => {
            let offset_in_section = section_offset(addresses, value).unwrap_or(0);
            let byte = data
                .get_mut(at)
                .ok_or_else(|| "relocation past the end of the section".to_string())?;
            let truncated = u8::try_from(offset_in_section & 0x7f).unwrap_or(0);
            *byte = (*byte & 0x80) | truncated;
            Ok(())
        }
        IMAGE_REL_AMD64_SECTION => {
            let index = match value {
                Value::Address { section, .. } if section != u32::MAX => {
                    u16::try_from(section.wrapping_add(1)).unwrap_or(0)
                }
                _ => 0,
            };
            let slot = data
                .get_mut(at..)
                .and_then(<[u8]>::first_chunk_mut::<2>)
                .ok_or_else(|| "relocation past the end of the section".to_string())?;
            *slot = index.to_le_bytes();
            Ok(())
        }
        _ => Err(unknown()),
    }
}

/// The offset of an address inside its output section.
fn section_offset(addresses: &Addresses<'_, '_>, value: Value) -> Option<u32> {
    let Value::Address { rva, section } = value else {
        return None;
    };
    let out = addresses.layout.sections.get(section as usize)?;
    rva.checked_sub(out.rva)
}

fn store32(data: &mut [u8], at: usize, value: u32) -> Result<(), String> {
    let slot = data
        .get_mut(at..)
        .and_then(<[u8]>::first_chunk_mut::<4>)
        .ok_or_else(|| "relocation past the end of the section".to_string())?;
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
