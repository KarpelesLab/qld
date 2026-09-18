//! Final addresses of symbols, atoms and synthetic entries, once the layout
//! is done.

#![deny(clippy::arithmetic_side_effects)]

use crate::error::{Error, Result};
use crate::ids::SymbolId;
use crate::macho::read::consts::{N_ABS, N_SECT};

use super::layout::{Layout, SectionKind};
use super::reloc::{Place, Resolve, Value};
use super::scan::Synthetic;
use super::state::{Link, NONE, SymbolDef};
use super::thunks::Thunks;

/// Address lookups over a finished layout.
pub struct Addresses<'x, 'a> {
    /// The resolved link.
    pub link: &'x Link<'a>,
    /// The layout.
    pub layout: &'x Layout,
    /// The synthetic tables.
    pub synthetic: &'x Synthetic,
    /// Range-extension thunks (arm64).
    pub thunks: &'x Thunks,
}

impl std::fmt::Debug for Addresses<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Addresses").finish_non_exhaustive()
    }
}

fn slot(section: Option<&super::layout::OutSection>, index: u32, size: u64) -> Option<u64> {
    if index == NONE {
        return None;
    }
    let section = section?;
    Some(
        section
            .addr
            .saturating_add(u64::from(index).saturating_mul(size)),
    )
}

impl Addresses<'_, '_> {
    /// The address of the Mach-O header.
    #[must_use]
    pub fn header_address(&self) -> u64 {
        self.layout
            .segment(b"__TEXT")
            .map_or(self.link.config.image_base, |s| s.vmaddr)
    }

    /// The value of a symbol table entry `symbol` of object `file`.
    #[must_use]
    pub fn object_symbol(&self, file: usize, symbol: u32) -> Option<Value> {
        let object = self.link.object(file)?;
        let entry = object.file.symbols().get(symbol).ok()?;
        match entry.n_type & 0x0e {
            N_ABS => Some(Value::Absolute(entry.n_value)),
            N_SECT => {
                let atom = object.atoms.symbol_atom(symbol)?;
                let info = object.atoms.atoms().get(atom)?;
                let section = object
                    .file
                    .sections()
                    .get(usize::from(entry.n_sect).checked_sub(1)?)?;
                let start = section.addr.checked_add(info.offset)?;
                let base = self.layout.atom_address(self.link.atom_id(file, atom))?;
                Some(Value::Address(
                    base.wrapping_add(entry.n_value.wrapping_sub(start)),
                ))
            }
            _ => None,
        }
    }

    /// The `__auth_got` slot of a symbol (arm64e stubs).
    #[must_use]
    pub fn auth_got(&self, id: SymbolId) -> Option<u64> {
        slot(
            self.layout.find(SectionKind::AuthGot),
            *self.synthetic.auth_got_index.get(id.index())?,
            8,
        )
    }

    /// The address of this image's own definition of an exported weak
    /// symbol that references bind through weak lookup.
    #[must_use]
    pub fn weak_definition(&self, id: SymbolId) -> Option<u64> {
        self.weak_import(id)?;
        match self.link.defs.get(id.index())? {
            SymbolDef::Object { file, symbol } => {
                match self.object_symbol(usize::try_from(*file).ok()?, *symbol)? {
                    Value::Address(address) | Value::Absolute(address) => Some(address),
                    Value::Import(..) => None,
                }
            }
            _ => None,
        }
    }

    /// The value of a global symbol: an address, an absolute value, or an
    /// import.
    #[must_use]
    pub fn symbol(&self, id: SymbolId) -> Option<Value> {
        match self.link.defs.get(id.index())? {
            SymbolDef::Object { file, symbol } => {
                if let Some(import) = self.weak_import(id) {
                    return Some(Value::Import(import, 0));
                }
                self.object_symbol(usize::try_from(*file).ok()?, *symbol)
            }
            SymbolDef::Common { .. } => {
                // In symbol order, so sorted.
                let index = self.synthetic.commons.binary_search(&id).ok()?;
                let section = self.layout.find(SectionKind::Common)?;
                Some(Value::Address(
                    section
                        .addr
                        .saturating_add(*self.layout.common_offset.get(index)?),
                ))
            }
            SymbolDef::Header | SymbolDef::DsoHandle => Some(Value::Address(self.header_address())),
            SymbolDef::Dylib { .. } | SymbolDef::DynamicLookup => {
                let import = *self.synthetic.import_index.get(id.index())?;
                (import != NONE).then_some(Value::Import(import, 0))
            }
            SymbolDef::Boundary {
                start,
                segment,
                section,
            } => {
                let address = match section {
                    Some(section) => self
                        .layout
                        .by_name(segment, section)
                        .map(|s| if *start { s.addr } else { s.end() }),
                    None => self.layout.segment(segment).map(|s| {
                        if *start {
                            s.vmaddr
                        } else {
                            s.vmaddr.saturating_add(s.vmsize)
                        }
                    }),
                };
                // A boundary of a missing section is zero-sized at the start
                // of `__TEXT`, as in ld64.
                Some(Value::Address(
                    address.unwrap_or_else(|| self.header_address()),
                ))
            }
            SymbolDef::Undefined => None,
        }
    }
}

impl Resolve for Addresses<'_, '_> {
    fn value(&self, place: Place, addend: i64) -> Result<Value> {
        match place {
            Place::Absolute(value) => Ok(Value::Absolute(value)),
            Place::Atom { atom, offset } => {
                let base = self.layout.atom_address(atom).ok_or_else(|| {
                    Error::Internal("relocation against an atom that is not in the output".into())
                })?;
                Ok(Value::Address(base.wrapping_add(offset as u64)))
            }
            Place::Symbol(id) => match self.symbol(id) {
                Some(Value::Address(address)) => {
                    Ok(Value::Address(address.wrapping_add(addend as u64)))
                }
                Some(Value::Absolute(value)) => {
                    Ok(Value::Absolute(value.wrapping_add(addend as u64)))
                }
                Some(Value::Import(import, _)) => Ok(Value::Import(import, addend)),
                None => Err(Error::Internal(format!(
                    "relocation against undefined symbol {}",
                    String::from_utf8_lossy(self.link.symbols.name(id).bytes())
                ))),
            },
        }
    }

    fn got(&self, id: SymbolId) -> Option<u64> {
        slot(
            self.layout.find(SectionKind::Got),
            *self.synthetic.got_index.get(id.index())?,
            8,
        )
    }

    fn local_got(&self, file: usize, symbol: u32) -> Option<u64> {
        let index = *self
            .synthetic
            .local_got_index
            .get(&(u32::try_from(file).ok()?, symbol))?;
        let global = u32::try_from(self.synthetic.got.len()).ok()?;
        slot(
            self.layout.find(SectionKind::Got),
            global.checked_add(index)?,
            8,
        )
    }

    fn stub(&self, id: SymbolId) -> Option<u64> {
        slot(
            self.layout.find(SectionKind::Stubs),
            *self.synthetic.stub_index.get(id.index())?,
            self.link.config.stub_size(),
        )
    }

    fn tlv_pointer(&self, id: SymbolId) -> Option<u64> {
        slot(
            self.layout.find(SectionKind::ThreadPtrs),
            *self.synthetic.tlv_index.get(id.index())?,
            8,
        )
    }

    fn tlv_template(&self) -> u64 {
        self.layout.tlv_template_start().unwrap_or(0)
    }

    fn thunk(&self, from: u64, target: u64) -> Option<u64> {
        self.thunks.find(from, target)
    }

    fn weak_import(&self, id: SymbolId) -> Option<u32> {
        if !self
            .synthetic
            .weak_bound
            .get(id.index())
            .copied()
            .unwrap_or(false)
        {
            return None;
        }
        let import = *self.synthetic.import_index.get(id.index())?;
        (import != NONE).then_some(import)
    }

    fn writable(&self, address: u64) -> bool {
        self.layout
            .segments
            .iter()
            .find(|s| address >= s.vmaddr && address < s.vmaddr.saturating_add(s.vmsize))
            .is_some_and(|s| s.initprot & 2 != 0)
    }
}
