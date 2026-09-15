//! The symbol table, string table, indirect symbol table, export list,
//! function starts and data-in-code entries.
//!
//! The symbol table follows `LC_DYSYMTAB`'s grouping: local symbols (the
//! STABS debug map first, then object locals and private externs), then
//! external definitions sorted by name, then undefined symbols sorted by
//! name. As in ld64 and lld, temporary labels (`l…`, `L…`) are left out.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::args::{DiscardMode, LinkOptions};
use crate::ids::SymbolId;
use crate::macho::read::consts::{
    BIND_SPECIAL_DYLIB_FLAT_LOOKUP, BIND_SPECIAL_DYLIB_MAIN_EXECUTABLE, DYNAMIC_LOOKUP_ORDINAL,
    EXECUTABLE_ORDINAL, EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE, EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL,
    EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION, N_ABS, N_EXT, N_PEXT, N_SECT, N_UNDF, N_WEAK_DEF,
    N_WEAK_REF, REFERENCED_DYNAMICALLY, S_THREAD_LOCAL_VARIABLES, SECTION_TYPE,
};

use super::addr::Addresses;
use super::buf::{pad_to, push_uleb, push16, push32, push64, to_usize};
use super::layout::OutSection;
use super::reloc::Value;
use super::state::{NONE, SymbolDef};
use super::trie::ExportEntry;

/// `INDIRECT_SYMBOL_LOCAL`.
pub const INDIRECT_SYMBOL_LOCAL: u32 = 0x8000_0000;
/// `INDIRECT_SYMBOL_ABS`.
pub const INDIRECT_SYMBOL_ABS: u32 = 0x4000_0000;

/// One `nlist_64` before encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nlist {
    /// Name.
    pub name: Vec<u8>,
    /// `n_type`.
    pub n_type: u8,
    /// `n_sect`.
    pub n_sect: u8,
    /// `n_desc`.
    pub n_desc: u16,
    /// `n_value`.
    pub n_value: u64,
}

/// The encoded tables.
#[derive(Clone, Debug, Default)]
pub struct Tables {
    /// `nlist_64` records.
    pub symbols: Vec<u8>,
    /// Number of symbols.
    pub count: u32,
    /// Local symbols (index 0, count).
    pub locals: u32,
    /// External definitions (index `locals`, count).
    pub extdefs: u32,
    /// Undefined symbols (index `locals + extdefs`, count).
    pub undefs: u32,
    /// The string table, padded to 8 bytes.
    pub strings: Vec<u8>,
    /// The indirect symbol table.
    pub indirect: Vec<u8>,
    /// Number of indirect entries.
    pub indirect_count: u32,
    /// `reserved1` of `__got`, `__thread_ptrs` and `__stubs`.
    pub got_first: u32,
    /// Index of the first `__thread_ptrs` entry.
    pub tlv_first: u32,
    /// Index of the first `__stubs` entry.
    pub stubs_first: u32,
    /// The export trie entries.
    pub exports: Vec<ExportEntry>,
}

/// A simple glob: `*` matches any run, `?` one byte.
#[must_use]
pub fn glob_match(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some(p);
                mark = n;
                p = p.saturating_add(1);
            }
            Some(&c) if c == b'?' || Some(&c) == name.get(n) => {
                p = p.saturating_add(1);
                n = n.saturating_add(1);
            }
            _ => match star {
                Some(s) => {
                    p = s.saturating_add(1);
                    mark = mark.saturating_add(1);
                    n = mark;
                }
                None => return false,
            },
        }
    }
    while pattern.get(p) == Some(&b'*') {
        p = p.saturating_add(1);
    }
    p == pattern.len()
}

/// Export filtering from `-exported_symbol(s_list)` and
/// `-unexported_symbol(s_list)`.
#[derive(Clone, Debug, Default)]
pub struct ExportFilter {
    exported: Vec<Vec<u8>>,
    unexported: Vec<Vec<u8>>,
    none: bool,
}

impl ExportFilter {
    /// Reads the patterns and lists of `options`.
    ///
    /// # Errors
    ///
    /// Unreadable list files.
    pub fn new(options: &LinkOptions) -> crate::Result<Self> {
        let darwin = &options.darwin;
        let read = |paths: &[std::path::PathBuf], out: &mut Vec<Vec<u8>>| -> crate::Result<()> {
            for path in paths {
                let text = std::fs::read(path).map_err(|e| crate::Error::io(path, e))?;
                for line in text.split(|&b| b == b'\n') {
                    let line = match line.iter().position(|&b| b == b'#') {
                        Some(hash) => line.get(..hash).unwrap_or(&[]),
                        None => line,
                    };
                    let line = line.trim_ascii();
                    if !line.is_empty() {
                        out.push(line.to_vec());
                    }
                }
            }
            Ok(())
        };
        let mut filter = Self {
            exported: darwin
                .exported_symbols
                .iter()
                .map(|s| s.as_bytes().to_vec())
                .collect(),
            unexported: darwin
                .unexported_symbols
                .iter()
                .map(|s| s.as_bytes().to_vec())
                .collect(),
            none: darwin.no_exported_symbols,
        };
        read(&darwin.exported_symbols_lists, &mut filter.exported)?;
        read(&darwin.unexported_symbols_lists, &mut filter.unexported)?;
        Ok(filter)
    }

    /// Whether a global definition named `name` is exported.
    #[must_use]
    pub fn exports(&self, name: &[u8]) -> bool {
        if self.none {
            return false;
        }
        if !self.exported.is_empty() && !self.exported.iter().any(|p| glob_match(p, name)) {
            return false;
        }
        !self.unexported.iter().any(|p| glob_match(p, name))
    }
}

fn ordinal_of(ordinal: i32) -> u16 {
    let byte = match ordinal {
        BIND_SPECIAL_DYLIB_FLAT_LOOKUP => DYNAMIC_LOOKUP_ORDINAL,
        BIND_SPECIAL_DYLIB_MAIN_EXECUTABLE => EXECUTABLE_ORDINAL,
        o => u8::try_from(o).unwrap_or(0),
    };
    u16::from(byte) << 8
}

/// The 1-based output section ordinal containing `address`.
fn section_ordinal(sections: &[OutSection], address: u64) -> u8 {
    sections
        .iter()
        .position(|s| address >= s.addr && address < s.end().max(s.addr.saturating_add(1)))
        .or_else(|| sections.iter().position(|s| address == s.end()))
        .and_then(|i| u8::try_from(i.saturating_add(1)).ok())
        .unwrap_or(0)
}

/// Builds the tables. `stabs` are the debug map entries (already in order).
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn build(
    addresses: &Addresses<'_, '_>,
    options: &LinkOptions,
    filter: &ExportFilter,
    stabs: Vec<Nlist>,
) -> Tables {
    let link = addresses.link;
    let layout = addresses.layout;
    let sections = &layout.sections;
    let config = link.config;
    let base = addresses.header_address();

    let mut locals: Vec<Nlist> = stabs;
    let mut extdefs: Vec<(SymbolId, Nlist)> = Vec::new();
    let mut undefs: Vec<(SymbolId, Nlist)> = Vec::new();
    let mut exports: Vec<ExportEntry> = Vec::new();

    // Object locals.
    if options.discard != DiscardMode::All {
        for (file, input) in link.files.iter().enumerate() {
            let Some(object) = link.object(file) else {
                continue;
            };
            for symbol in object.file.symbols().iter().flatten() {
                if symbol.is_external() || symbol.n_type & 0x0e != N_SECT {
                    continue;
                }
                if symbol.name.is_empty()
                    || symbol.name.starts_with(b"l")
                    || symbol.name.starts_with(b"L")
                {
                    continue;
                }
                let Some(Value::Address(address)) = addresses.object_symbol(file, symbol.index)
                else {
                    continue;
                };
                let Some(atom) = object.atoms.symbol_atom(symbol.index) else {
                    continue;
                };
                if !link.is_live(file, atom) {
                    continue;
                }
                let _ = input;
                locals.push(Nlist {
                    name: symbol.name.to_vec(),
                    n_type: N_SECT,
                    n_sect: section_ordinal(sections, address),
                    n_desc: symbol.n_desc & !(N_WEAK_DEF | N_WEAK_REF),
                    n_value: address,
                });
            }
        }
    }

    // Globals.
    for index in 0..link.symbols.len() {
        let id = SymbolId::new(index);
        let name = link.symbols.name(id).bytes();
        match link.defs.get(index) {
            Some(SymbolDef::Object { file, symbol }) => {
                let file = to_usize(u64::from(*file));
                let Some(object) = link.object(file) else {
                    continue;
                };
                let Ok(entry) = object.file.symbols().get(*symbol) else {
                    continue;
                };
                let Some(value) = addresses.object_symbol(file, *symbol) else {
                    continue;
                };
                if entry.n_type & 0x0e == N_SECT {
                    let live = object
                        .atoms
                        .symbol_atom(*symbol)
                        .is_some_and(|atom| link.is_live(file, atom));
                    if !live {
                        continue;
                    }
                }
                let hidden = entry.is_private_external()
                    || link.files.get(file).is_some_and(|f| f.hidden)
                    || !filter.exports(name);
                let (n_type, n_sect, address) = match value {
                    Value::Absolute(v) => (N_ABS, 0, v),
                    Value::Address(a) => (N_SECT, section_ordinal(sections, a), a),
                    Value::Import(..) => continue,
                };
                let weak = entry.is_weak_def();
                let mut n_desc = entry.n_desc & (REFERENCED_DYNAMICALLY | 0x0020);
                if weak {
                    n_desc |= N_WEAK_DEF;
                }
                if hidden {
                    if options.discard == DiscardMode::All {
                        continue;
                    }
                    locals.push(Nlist {
                        name: name.to_vec(),
                        n_type: n_type | N_PEXT,
                        n_sect,
                        n_desc,
                        n_value: address,
                    });
                    continue;
                }
                let tlv = sections
                    .get(usize::from(n_sect).saturating_sub(1))
                    .is_some_and(|s| s.flags & SECTION_TYPE == S_THREAD_LOCAL_VARIABLES)
                    && n_sect != 0;
                let mut flags = 0u64;
                if weak {
                    flags |= EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION;
                }
                if tlv {
                    flags |= EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL;
                }
                let export_address = if n_type == N_ABS {
                    flags |= EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE;
                    address
                } else {
                    address.wrapping_sub(base)
                };
                exports.push(ExportEntry {
                    name: name.to_vec(),
                    flags,
                    address: export_address,
                });
                extdefs.push((
                    id,
                    Nlist {
                        name: name.to_vec(),
                        n_type: n_type | N_EXT,
                        n_sect,
                        n_desc,
                        n_value: address,
                    },
                ));
            }
            Some(SymbolDef::Common { .. }) => {
                let Some(Value::Address(address)) = addresses.symbol(id) else {
                    continue;
                };
                let n_sect = section_ordinal(sections, address);
                if filter.exports(name) {
                    exports.push(ExportEntry {
                        name: name.to_vec(),
                        flags: 0,
                        address: address.wrapping_sub(base),
                    });
                    extdefs.push((
                        id,
                        Nlist {
                            name: name.to_vec(),
                            n_type: N_SECT | N_EXT,
                            n_sect,
                            n_desc: 0,
                            n_value: address,
                        },
                    ));
                } else {
                    locals.push(Nlist {
                        name: name.to_vec(),
                        n_type: N_SECT | N_PEXT,
                        n_sect,
                        n_desc: 0,
                        n_value: address,
                    });
                }
            }
            Some(SymbolDef::Header) => {
                if config.is_exec() {
                    exports.push(ExportEntry {
                        name: name.to_vec(),
                        flags: 0,
                        address: 0,
                    });
                    extdefs.push((
                        id,
                        Nlist {
                            name: name.to_vec(),
                            n_type: N_SECT | N_EXT,
                            n_sect: 1,
                            n_desc: REFERENCED_DYNAMICALLY,
                            n_value: base,
                        },
                    ));
                }
            }
            Some(SymbolDef::Dylib { weak, .. }) => {
                let import = addresses
                    .synthetic
                    .import_index
                    .get(index)
                    .copied()
                    .unwrap_or(NONE);
                let Some(entry) = addresses.synthetic.imports.get(to_usize(u64::from(import)))
                else {
                    continue;
                };
                let mut n_desc = ordinal_of(entry.ordinal);
                if *weak {
                    n_desc |= N_WEAK_DEF;
                }
                if entry.weak {
                    n_desc |= N_WEAK_REF;
                }
                undefs.push((
                    id,
                    Nlist {
                        name: name.to_vec(),
                        n_type: N_UNDF | N_EXT,
                        n_sect: 0,
                        n_desc,
                        n_value: 0,
                    },
                ));
            }
            Some(SymbolDef::DynamicLookup) => {
                if addresses.synthetic.import_index.get(index) == Some(&NONE) {
                    continue;
                }
                undefs.push((
                    id,
                    Nlist {
                        name: name.to_vec(),
                        n_type: N_UNDF | N_EXT,
                        n_sect: 0,
                        n_desc: ordinal_of(BIND_SPECIAL_DYLIB_FLAT_LOOKUP),
                        n_value: 0,
                    },
                ));
            }
            _ => {}
        }
    }
    extdefs.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    undefs.sort_by(|a, b| a.1.name.cmp(&b.1.name));

    // Encode.
    let mut tables = Tables::default();
    let mut strings: Vec<u8> = vec![b' ', 0];
    let mut string_offsets: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut symtab_index: HashMap<SymbolId, u32> = HashMap::new();
    let mut count = 0u32;
    let mut emit = |entry: &Nlist, tables: &mut Tables, strings: &mut Vec<u8>| {
        let strx = if entry.name.is_empty() {
            0
        } else if let Some(&offset) = string_offsets.get(&entry.name) {
            offset
        } else {
            let offset = u32::try_from(strings.len()).unwrap_or(0);
            strings.extend_from_slice(&entry.name);
            strings.push(0);
            string_offsets.insert(entry.name.clone(), offset);
            offset
        };
        push32(&mut tables.symbols, strx);
        tables.symbols.push(entry.n_type);
        tables.symbols.push(entry.n_sect);
        push16(&mut tables.symbols, entry.n_desc);
        push64(&mut tables.symbols, entry.n_value);
    };
    for entry in &locals {
        emit(entry, &mut tables, &mut strings);
        count = count.saturating_add(1);
    }
    tables.locals = count;
    for (id, entry) in &extdefs {
        emit(entry, &mut tables, &mut strings);
        symtab_index.insert(*id, count);
        count = count.saturating_add(1);
    }
    tables.extdefs = count.saturating_sub(tables.locals);
    for (id, entry) in &undefs {
        emit(entry, &mut tables, &mut strings);
        symtab_index.insert(*id, count);
        count = count.saturating_add(1);
    }
    tables.undefs = count
        .saturating_sub(tables.locals)
        .saturating_sub(tables.extdefs);
    tables.count = count;
    pad_to(&mut strings, 8);
    tables.strings = strings;

    // Indirect symbols: __got, __thread_ptrs, __stubs.
    let indirect_entry = |id: SymbolId| -> u32 {
        match (link.is_imported(id), symtab_index.get(&id)) {
            (true, Some(&index)) => index,
            (false, Some(&index)) if !matches!(addresses.symbol(id), Some(Value::Absolute(_))) => {
                let _ = index;
                INDIRECT_SYMBOL_LOCAL
            }
            (_, _) if matches!(addresses.symbol(id), Some(Value::Absolute(_))) => {
                INDIRECT_SYMBOL_LOCAL | INDIRECT_SYMBOL_ABS
            }
            _ => INDIRECT_SYMBOL_LOCAL,
        }
    };
    let synthetic = addresses.synthetic;
    tables.got_first = 0;
    for &id in &synthetic.got {
        push32(&mut tables.indirect, indirect_entry(id));
    }
    tables.tlv_first = u32::try_from(synthetic.got.len()).unwrap_or(0);
    for &id in &synthetic.thread_ptrs {
        push32(&mut tables.indirect, indirect_entry(id));
    }
    tables.stubs_first = tables
        .tlv_first
        .saturating_add(u32::try_from(synthetic.thread_ptrs.len()).unwrap_or(0));
    for &id in &synthetic.stubs {
        push32(&mut tables.indirect, indirect_entry(id));
    }
    tables.indirect_count = u32::try_from(tables.indirect.len() / 4).unwrap_or(0);
    pad_to(&mut tables.indirect, 8);
    tables.exports = exports;
    tables
}

/// `LC_FUNCTION_STARTS`: ULEB128 deltas between the addresses of the
/// defined symbols in code sections, starting from `__TEXT`.
#[must_use]
pub fn function_starts(addresses: &Addresses<'_, '_>) -> Vec<u8> {
    let link = addresses.link;
    let sections = &addresses.layout.sections;
    let mut starts: Vec<u64> = Vec::new();
    for file in 0..link.files.len() {
        let Some(object) = link.object(file) else {
            continue;
        };
        for symbol in object.file.symbols().iter().flatten() {
            if symbol.n_type & 0x0e != N_SECT {
                continue;
            }
            let Some(section) = object.file.section_by_ordinal(u32::from(symbol.n_sect)) else {
                continue;
            };
            if !section.has_code() {
                continue;
            }
            if symbol.is_external() {
                let global = object
                    .global_of_symbol
                    .get(to_usize(u64::from(symbol.index)))
                    .copied()
                    .unwrap_or(NONE);
                if !link.global_wins(file, to_usize(u64::from(global))) {
                    continue;
                }
            }
            let Some(atom) = object.atoms.symbol_atom(symbol.index) else {
                continue;
            };
            if !link.is_live(file, atom) {
                continue;
            }
            if let Some(Value::Address(address)) = addresses.object_symbol(file, symbol.index)
                && sections
                    .iter()
                    .any(|s| s.has_code() && address >= s.addr && address < s.end())
            {
                starts.push(address);
            }
        }
    }
    starts.sort_unstable();
    starts.dedup();
    let mut out = Vec::new();
    let mut last = addresses.header_address();
    for address in starts {
        push_uleb(&mut out, address.saturating_sub(last));
        last = address;
    }
    if !out.is_empty() {
        out.push(0);
    }
    pad_to(&mut out, 8);
    out
}

/// `LC_DATA_IN_CODE` entries, rebased to offsets from the header.
#[must_use]
pub fn data_in_code(addresses: &Addresses<'_, '_>) -> Vec<u8> {
    let link = addresses.link;
    let base = addresses.header_address();
    let mut entries: Vec<(u64, u16, u16)> = Vec::new();
    for file in 0..link.files.len() {
        let Some(object) = link.object(file) else {
            continue;
        };
        for entry in object.file.data_in_code() {
            let address = u64::from(entry.offset);
            let Some(section) = object.file.section_at_address(address) else {
                continue;
            };
            let Some(header) = object.file.sections().get(section) else {
                continue;
            };
            let offset = address.saturating_sub(header.addr);
            let Some(atom) = super::state::atom_containing(object, section, offset) else {
                continue;
            };
            if !link.is_live(file, atom) {
                continue;
            }
            let Some(start) = addresses.layout.atom_address(link.atom_id(file, atom)) else {
                continue;
            };
            let atom_offset = object.atoms.atoms().get(atom).map_or(0, |a| a.offset);
            let output = start.saturating_add(offset.saturating_sub(atom_offset));
            entries.push((output.saturating_sub(base), entry.length, entry.kind));
        }
    }
    entries.sort_unstable();
    let mut out = Vec::with_capacity(entries.len().saturating_mul(8));
    for (offset, length, kind) in entries {
        push32(&mut out, u32::try_from(offset).unwrap_or(0));
        push16(&mut out, length);
        push16(&mut out, kind);
    }
    out
}
