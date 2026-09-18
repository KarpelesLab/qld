//! Relocations: decoding them into referents and addends, and applying
//! them for arm64 and x86_64.
//!
//! # Addends
//!
//! arm64 keeps addends in `ARM64_RELOC_ADDEND` pairs, except `UNSIGNED`
//! (and the `SUBTRACTOR` pair), whose addend is the stored word. x86_64
//! stores every addend in the relocated field. A PC-relative x86_64 field
//! is relative to the end of the 4-byte field plus the 1, 2 or 4 bytes
//! `SIGNED_1`/`_2`/`_4` announce; for a symbol relocation the assembler has
//! already subtracted that distance `n` from the stored addend, so the
//! field holds `S + A - (P + 4 + n)` with `A` = stored + `n`. Decoding adds
//! `n` back, so that the target `S + A` is the real one (a store of an
//! immediate to the first byte of a static would otherwise land one to four
//! bytes before its atom).
//!
//! A relocation without `r_extern` names a section, and the stored value
//! locates the target by its address in the object: the value itself for
//! `UNSIGNED`, and `P + 4 + n + value` for PC-relative x86_64 fields. A
//! relocation against a local symbol is turned into an address the same
//! way when the addend moves it out of the symbol's atom, so references
//! into literal sections (which are split into one atom per literal) land
//! on the right literal.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::aarch64;
use crate::error::{Error, Result};
use crate::ids::SymbolId;
use crate::macho::read::consts::{
    ARM64_RELOC_ADDEND, ARM64_RELOC_AUTHENTICATED_POINTER, ARM64_RELOC_BRANCH26,
    ARM64_RELOC_GOT_LOAD_PAGE21, ARM64_RELOC_GOT_LOAD_PAGEOFF12, ARM64_RELOC_PAGE21,
    ARM64_RELOC_PAGEOFF12, ARM64_RELOC_POINTER_TO_GOT, ARM64_RELOC_SUBTRACTOR,
    ARM64_RELOC_TLVP_LOAD_PAGE21, ARM64_RELOC_TLVP_LOAD_PAGEOFF12, ARM64_RELOC_UNSIGNED,
    CPU_TYPE_ARM64, N_ABS, N_SECT, S_THREAD_LOCAL_VARIABLES, X86_64_RELOC_BRANCH, X86_64_RELOC_GOT,
    X86_64_RELOC_GOT_LOAD, X86_64_RELOC_SIGNED, X86_64_RELOC_SIGNED_1, X86_64_RELOC_SIGNED_2,
    X86_64_RELOC_SIGNED_4, X86_64_RELOC_SUBTRACTOR, X86_64_RELOC_TLV, X86_64_RELOC_UNSIGNED,
};
use crate::macho::read::{PairedRelocation, RelocationTarget};

use super::buf::{get32, get64, to_usize};
use super::object::{LinkObject, NOT_GLOBAL};
use super::state::{Link, SymbolDef, atom_containing};

/// What a relocation refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Referent {
    /// A global symbol.
    Global(SymbolId),
    /// A local symbol of the same object (symbol table index).
    Local(u32),
    /// An address in section `section` (0-based) of the same object.
    Address {
        /// Section index.
        section: usize,
        /// Address in the object.
        address: u64,
    },
}

/// A decoded relocation.
#[derive(Clone, Copy, Debug)]
pub struct Decoded {
    /// Offset of the field within its input section.
    pub offset: u64,
    /// Relocation type.
    pub r_type: u8,
    /// Field size: 0, 1, 2 (4 bytes) or 3 (8 bytes).
    pub length: u8,
    /// PC-relative.
    pub pcrel: bool,
    /// The target.
    pub referent: Referent,
    /// The addend.
    pub addend: i64,
    /// For a `SUBTRACTOR` pair, what is subtracted.
    pub subtrahend: Option<Referent>,
}

/// The extra PC offset of x86_64 `SIGNED_n` relocations.
fn pcrel_extra(r_type: u8) -> i64 {
    match r_type {
        X86_64_RELOC_SIGNED_1 => 1,
        X86_64_RELOC_SIGNED_2 => 2,
        X86_64_RELOC_SIGNED_4 => 4,
        _ => 0,
    }
}

fn stored(data: &[u8], offset: usize, length: u8) -> i64 {
    match length {
        3 => get64(data, offset).map_or(0, |v| v as i64),
        2 => get32(data, offset).map_or(0, |v| i64::from(v as i32)),
        _ => 0,
    }
}

/// Decodes one relocation of section `section` of `object`.
///
/// # Errors
///
/// `Error::Malformed` for relocations qld does not understand.
pub fn decode(
    link: &Link<'_>,
    file: usize,
    object: &LinkObject<'_>,
    section: usize,
    data: &[u8],
    relocation: &PairedRelocation,
) -> Result<Decoded> {
    let reloc = relocation.relocation;
    let arm64 = object.file.header().cpu_type == CPU_TYPE_ARM64;
    let header = object.file.sections().get(section);
    let section_addr = header.map_or(0, |s| s.addr);
    let offset = u64::from(reloc.address);
    let at = to_usize(offset);
    let fail = |what: &str| {
        object.file.source().malformed(
            header.map_or(0, |s| u64::from(s.reloff)),
            format!(
                "relocation at {:#x} in {} ({what})",
                offset,
                header.map_or_else(String::new, |s| s.display_name())
            ),
        )
    };
    let symbol_referent = |target: RelocationTarget| -> Result<Option<Referent>> {
        match target {
            RelocationTarget::Symbol(symbol) => {
                let global = object
                    .global_of_symbol
                    .get(to_usize(u64::from(symbol)))
                    .copied()
                    .ok_or_else(|| fail("symbol index out of range"))?;
                if global == NOT_GLOBAL {
                    Ok(Some(Referent::Local(symbol)))
                } else {
                    link.global_id(file, global)
                        .map(|id| Some(Referent::Global(id)))
                        .ok_or_else(|| fail("global symbol without an ID"))
                }
            }
            RelocationTarget::Section(_) => Ok(None),
            RelocationTarget::Scattered(_) => Err(fail("scattered relocation")),
        }
    };
    let section_of = |target: RelocationTarget| -> Result<usize> {
        match target {
            RelocationTarget::Section(ordinal) => usize::try_from(ordinal)
                .ok()
                .and_then(|o| o.checked_sub(1))
                .filter(|&s| s < object.file.sections().len())
                .ok_or_else(|| fail("section ordinal out of range")),
            _ => Err(fail("expected a section relocation")),
        }
    };

    let subtrahend = match relocation.subtractor {
        Some(sub) => match symbol_referent(sub.target)? {
            Some(referent) => Some(referent),
            None => return Err(fail("SUBTRACTOR against a section")),
        },
        None => None,
    };

    let embedded = stored(data, at, reloc.length);
    let (referent, addend) = if arm64 {
        match reloc.r_type {
            ARM64_RELOC_UNSIGNED => match symbol_referent(reloc.target)? {
                Some(referent) => (referent, embedded),
                None => (
                    Referent::Address {
                        section: section_of(reloc.target)?,
                        address: embedded as u64,
                    },
                    0,
                ),
            },
            ARM64_RELOC_BRANCH26 => match symbol_referent(reloc.target)? {
                Some(referent) => (referent, i64::from(relocation.addend.unwrap_or(0))),
                None => {
                    let insn = get32(data, at).unwrap_or(0);
                    let imm = i64::from(((insn & 0x03ff_ffff) << 6) as i32 >> 6);
                    let address = section_addr
                        .wrapping_add(offset)
                        .wrapping_add(imm.wrapping_mul(4) as u64);
                    (
                        Referent::Address {
                            section: section_of(reloc.target)?,
                            address,
                        },
                        0,
                    )
                }
            },
            ARM64_RELOC_PAGE21
            | ARM64_RELOC_PAGEOFF12
            | ARM64_RELOC_GOT_LOAD_PAGE21
            | ARM64_RELOC_GOT_LOAD_PAGEOFF12
            | ARM64_RELOC_TLVP_LOAD_PAGE21
            | ARM64_RELOC_TLVP_LOAD_PAGEOFF12
            | ARM64_RELOC_POINTER_TO_GOT => match symbol_referent(reloc.target)? {
                Some(referent) => (referent, i64::from(relocation.addend.unwrap_or(0))),
                None => return Err(fail("section-relative page relocation")),
            },
            ARM64_RELOC_AUTHENTICATED_POINTER => {
                return Err(fail("pointer authentication (arm64e) is not supported"));
            }
            ARM64_RELOC_SUBTRACTOR | ARM64_RELOC_ADDEND => {
                return Err(fail("unpaired relocation"));
            }
            _ => return Err(fail("unknown relocation type")),
        }
    } else {
        match reloc.r_type {
            X86_64_RELOC_UNSIGNED => match symbol_referent(reloc.target)? {
                Some(referent) => (referent, embedded),
                None => (
                    Referent::Address {
                        section: section_of(reloc.target)?,
                        address: embedded as u64,
                    },
                    0,
                ),
            },
            X86_64_RELOC_SIGNED
            | X86_64_RELOC_SIGNED_1
            | X86_64_RELOC_SIGNED_2
            | X86_64_RELOC_SIGNED_4
            | X86_64_RELOC_BRANCH
            | X86_64_RELOC_GOT_LOAD
            | X86_64_RELOC_GOT
            | X86_64_RELOC_TLV => {
                let extra = pcrel_extra(reloc.r_type);
                match symbol_referent(reloc.target)? {
                    // The stored addend has the `SIGNED_n` distance taken
                    // out; put it back so the target is the real one.
                    Some(referent) => (referent, embedded.wrapping_add(extra)),
                    None => {
                        let address = section_addr
                            .wrapping_add(offset)
                            .wrapping_add(4)
                            .wrapping_add(extra as u64)
                            .wrapping_add(embedded as u64);
                        (
                            Referent::Address {
                                section: section_of(reloc.target)?,
                                address,
                            },
                            0,
                        )
                    }
                }
            }
            X86_64_RELOC_SUBTRACTOR => return Err(fail("unpaired SUBTRACTOR")),
            _ => return Err(fail("unknown relocation type")),
        }
    };
    Ok(Decoded {
        offset,
        r_type: reloc.r_type,
        length: reloc.length,
        pcrel: reloc.pcrel,
        referent,
        addend,
        subtrahend,
    })
}

/// Where a referent is, before layout: an atom and an offset in it, or a
/// global symbol that is not defined in an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// Offset `offset` (possibly negative or past the end) of atom `atom`
    /// (global numbering), with the addend already folded in.
    Atom {
        /// Global atom index.
        atom: usize,
        /// Offset within the atom.
        offset: i64,
    },
    /// A global not defined in an object; the addend is not folded in.
    Symbol(SymbolId),
    /// An absolute value, addend folded in.
    Absolute(u64),
}

/// Locates `referent` of file `file` plus `addend`.
///
/// # Errors
///
/// `Error::Malformed` when the referent does not exist.
pub fn place(
    link: &Link<'_>,
    file: usize,
    object: &LinkObject<'_>,
    referent: Referent,
    addend: i64,
) -> Result<Place> {
    let fail = |what: String| object.file.source().malformed(0, what);
    let in_section = |section: usize, address: u64| -> Result<Place> {
        let header = object
            .file
            .sections()
            .get(section)
            .ok_or_else(|| fail(format!("section {section} out of range")))?;
        let offset = address
            .checked_sub(header.addr)
            .filter(|&o| o <= header.size)
            .ok_or_else(|| {
                fail(format!(
                    "relocation target {address:#x} (outside section {})",
                    header.display_name()
                ))
            })?;
        let atom = atom_containing(object, section, offset).ok_or_else(|| {
            fail(format!(
                "relocation target {address:#x} (no atom in {})",
                header.display_name()
            ))
        })?;
        let start = object.atoms.atoms().get(atom).map_or(0, |a| a.offset);
        Ok(Place::Atom {
            atom: link.atom_id(file, atom),
            offset: offset.wrapping_sub(start) as i64,
        })
    };
    match referent {
        Referent::Address { section, address } => {
            in_section(section, address.wrapping_add(addend as u64))
        }
        Referent::Local(symbol) => {
            let entry = object.file.symbols().get(symbol)?;
            match entry.n_type & 0x0e {
                N_ABS => Ok(Place::Absolute(entry.n_value.wrapping_add(addend as u64))),
                N_SECT => {
                    let section = usize::from(entry.n_sect)
                        .checked_sub(1)
                        .ok_or_else(|| fail(format!("symbol {symbol} has no section")))?;
                    let target = entry.n_value.wrapping_add(addend as u64);
                    // Stay with the symbol's atom when the target is inside
                    // it (or at its end).
                    if let Some(atom) = object.atoms.symbol_atom(symbol)
                        && let Some(info) = object.atoms.atoms().get(atom)
                        && let Some(header) = object.file.sections().get(section)
                    {
                        let start = header.addr.saturating_add(info.offset);
                        let end = start.saturating_add(info.size);
                        if target >= start && target <= end {
                            return Ok(Place::Atom {
                                atom: link.atom_id(file, atom),
                                offset: target.wrapping_sub(start) as i64,
                            });
                        }
                    }
                    in_section(section, target)
                }
                _ => Err(fail(format!(
                    "relocation against undefined local symbol {}",
                    String::from_utf8_lossy(entry.name)
                ))),
            }
        }
        Referent::Global(id) => match link.defs.get(id.index()) {
            Some(SymbolDef::Object {
                file: def_file,
                symbol,
            }) => {
                let def_file = usize::try_from(*def_file).unwrap_or(usize::MAX);
                let Some(def_object) = link.object(def_file) else {
                    return Ok(Place::Symbol(id));
                };
                let entry = def_object.file.symbols().get(*symbol)?;
                if entry.n_type & 0x0e == N_ABS {
                    return Ok(Place::Absolute(entry.n_value.wrapping_add(addend as u64)));
                }
                let atom = def_object.atoms.symbol_atom(*symbol).ok_or_else(|| {
                    fail(format!(
                        "symbol {} has no atom",
                        String::from_utf8_lossy(entry.name)
                    ))
                })?;
                let info = def_object.atoms.atoms().get(atom);
                let section = def_object
                    .file
                    .sections()
                    .get(usize::from(entry.n_sect).saturating_sub(1));
                let start = match (info, section) {
                    (Some(info), Some(section)) => section.addr.saturating_add(info.offset),
                    _ => 0,
                };
                Ok(Place::Atom {
                    atom: link.atom_id(def_file, atom),
                    offset: entry
                        .n_value
                        .wrapping_sub(start)
                        .wrapping_add(addend as u64) as i64,
                })
            }
            _ => Ok(Place::Symbol(id)),
        },
    }
}

/// A pointer the dynamic loader must fix up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixupKind {
    /// Slide the pointer: its unslid target address.
    Rebase(u64),
    /// Bind to import `import` plus `addend`.
    Bind {
        /// Import index.
        import: u32,
        /// Addend.
        addend: i64,
    },
}

/// A pointer fixup at address `address`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fixup {
    /// Address of the pointer.
    pub address: u64,
    /// What to do.
    pub kind: FixupKind,
}

/// The value a relocation computes, after indirection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    /// An address in the image.
    Address(u64),
    /// An absolute value.
    Absolute(u64),
    /// Import `index` (for pointers only), with this addend.
    Import(u32, i64),
}

/// What [`apply`] needs from the layout and the synthetic tables.
pub trait Resolve {
    /// The address (or import) of `place`.
    ///
    /// # Errors
    ///
    /// When the place is not in the output (a dead atom).
    fn value(&self, place: Place, addend: i64) -> Result<Value>;
    /// The `__got` slot of a symbol.
    fn got(&self, id: SymbolId) -> Option<u64>;
    /// The `__stubs` entry of a symbol.
    fn stub(&self, id: SymbolId) -> Option<u64>;
    /// The `__thread_ptrs` slot of a symbol.
    fn tlv_pointer(&self, id: SymbolId) -> Option<u64>;
    /// The start of the thread-local template.
    fn tlv_template(&self) -> u64;
    /// A range-extension thunk for a branch from `from` to `target`.
    fn thunk(&self, from: u64, target: u64) -> Option<u64>;
    /// Whether address `address` is in a writable segment.
    fn writable(&self, address: u64) -> bool;
    /// For an exported weak definition bound through dyld's weak lookup,
    /// its import index.
    fn weak_import(&self, id: SymbolId) -> Option<u32>;
}

fn range_error(what: &str, value: i64, place: u64) -> Error {
    Error::Limit(format!(
        "{what} relocation at {place:#x} out of range (value {value:#x})"
    ))
}

fn put_insn(out: &mut [u8], at: usize, insn: u32) -> Result<()> {
    aarch64::write_insn(out, at, insn)
        .ok_or_else(|| Error::Internal("relocation outside its section".into()))
}

fn put32(out: &mut [u8], at: usize, value: u32) -> Result<()> {
    super::buf::put32(out, at, value)
        .ok_or_else(|| Error::Internal("relocation outside its section".into()))
}

fn put64(out: &mut [u8], at: usize, value: u64) -> Result<()> {
    super::buf::put64(out, at, value)
        .ok_or_else(|| Error::Internal("relocation outside its section".into()))
}

fn page(address: u64) -> i64 {
    (address & !0xfff) as i64
}

/// The address a referent's value denotes, for PC-relative and instruction
/// fields (imports are an error there).
fn address_of(value: Value, what: &str, place: u64) -> Result<u64> {
    match value {
        Value::Address(address) | Value::Absolute(address) => Ok(address),
        Value::Import(..) => Err(Error::Limit(format!(
            "{what} relocation at {place:#x} refers to a symbol bound at run time"
        ))),
    }
}

/// Applies one decoded relocation to `out`, the bytes of its output
/// section, at offset `at`; `place_address` is the address of the field.
/// Returns the pointer fixup the field needs, if any.
///
/// # Errors
///
/// Out-of-range values and unsupported combinations.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn apply(
    arm64: bool,
    decoded: &Decoded,
    target: Place,
    subtrahend: Option<Place>,
    section_type: u32,
    resolve: &dyn Resolve,
    out: &mut [u8],
    at: usize,
    place_address: u64,
) -> Result<Option<Fixup>> {
    let addend = decoded.addend;
    // The global symbol, for `__got`, stub and thread-local pointer lookups
    // (which exist whatever defines the symbol).
    let symbol = match decoded.referent {
        Referent::Global(id) => Some(id),
        _ => None,
    };

    // SUBTRACTOR pairs: a difference, never a fixup.
    if let Some(subtrahend) = subtrahend {
        let minuend = address_of(resolve.value(target, addend)?, "SUBTRACTOR", place_address)?;
        let base = address_of(resolve.value(subtrahend, 0)?, "SUBTRACTOR", place_address)?;
        let value = minuend.wrapping_sub(base);
        return match decoded.length {
            3 => put64(out, at, value).map(|()| None),
            2 => {
                let signed = value as i64;
                if i32::try_from(signed).is_err() && u32::try_from(value).is_err() {
                    return Err(range_error("SUBTRACTOR", signed, place_address));
                }
                put32(out, at, value as u32).map(|()| None)
            }
            _ => Err(Error::Limit(format!(
                "SUBTRACTOR relocation at {place_address:#x} with an unsupported size"
            ))),
        };
    }

    let is_unsigned = (arm64 && decoded.r_type == ARM64_RELOC_UNSIGNED)
        || (!arm64 && decoded.r_type == X86_64_RELOC_UNSIGNED);
    if is_unsigned {
        let mut value = resolve.value(target, addend)?;
        // The offset field of a thread-local variable descriptor.
        if section_type == S_THREAD_LOCAL_VARIABLES && decoded.offset % 24 == 16 {
            let address = address_of(value, "thread-local offset", place_address)?;
            return put64(out, at, address.wrapping_sub(resolve.tlv_template())).map(|()| None);
        }
        // Pointers to exported weak definitions bind through weak lookup.
        if let Some(import) = symbol.and_then(|id| resolve.weak_import(id)) {
            value = Value::Import(import, addend);
        }
        return match (decoded.length, value) {
            (3, Value::Absolute(v)) => put64(out, at, v).map(|()| None),
            (3, Value::Address(address)) => {
                if !resolve.writable(place_address) {
                    return Err(Error::Limit(format!(
                        "pointer at {place_address:#x} in a read-only segment needs a rebase"
                    )));
                }
                put64(out, at, address)?;
                Ok(Some(Fixup {
                    address: place_address,
                    kind: FixupKind::Rebase(address),
                }))
            }
            (3, Value::Import(import, addend)) => {
                if !resolve.writable(place_address) {
                    return Err(Error::Limit(format!(
                        "pointer at {place_address:#x} in a read-only segment needs a bind"
                    )));
                }
                put64(out, at, 0)?;
                Ok(Some(Fixup {
                    address: place_address,
                    kind: FixupKind::Bind { import, addend },
                }))
            }
            (2, Value::Absolute(v)) => {
                if u32::try_from(v).is_err() && i32::try_from(v as i64).is_err() {
                    return Err(range_error("32-bit absolute", v as i64, place_address));
                }
                put32(out, at, v as u32).map(|()| None)
            }
            _ => Err(Error::Limit(format!(
                "32-bit absolute address at {place_address:#x} in a position-independent image"
            ))),
        };
    }

    if arm64 {
        let insn = aarch64::read_insn(out, at)
            .ok_or_else(|| Error::Internal("relocation outside its section".into()))?;
        match decoded.r_type {
            ARM64_RELOC_BRANCH26 => {
                let destination = match symbol.and_then(|id| resolve.stub(id)) {
                    Some(stub) => stub.wrapping_add(addend as u64),
                    None => address_of(resolve.value(target, addend)?, "BRANCH26", place_address)?,
                };
                let destination = if aarch64::branch_in_range(place_address, destination) {
                    destination
                } else {
                    resolve.thunk(place_address, destination).ok_or_else(|| {
                        range_error(
                            "BRANCH26",
                            destination.wrapping_sub(place_address) as i64,
                            place_address,
                        )
                    })?
                };
                let delta = destination.wrapping_sub(place_address) as i64;
                let insn = aarch64::Field::Branch26
                    .encode(insn, delta)
                    .map_err(|_| range_error("BRANCH26", delta, place_address))?;
                put_insn(out, at, insn)?;
            }
            ARM64_RELOC_PAGE21 | ARM64_RELOC_GOT_LOAD_PAGE21 | ARM64_RELOC_TLVP_LOAD_PAGE21 => {
                let (destination, _) = indirect(
                    decoded.r_type,
                    symbol,
                    target,
                    addend,
                    resolve,
                    place_address,
                )?;
                let delta = page(destination).wrapping_sub(page(place_address));
                let insn = aarch64::Field::Adrp21
                    .encode(insn, delta)
                    .map_err(|_| range_error("PAGE21", delta, place_address))?;
                put_insn(out, at, insn)?;
            }
            ARM64_RELOC_PAGEOFF12
            | ARM64_RELOC_GOT_LOAD_PAGEOFF12
            | ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => {
                let (destination, relaxed) = indirect(
                    decoded.r_type,
                    symbol,
                    target,
                    addend,
                    resolve,
                    place_address,
                )?;
                let mut insn = insn;
                if relaxed {
                    // `ldr Xt, [Xn, #off]` becomes `add Xt, Xn, #off`.
                    if insn & 0xbfc0_0000 != 0xb940_0000 {
                        return Err(Error::Limit(format!(
                            "relocation at {place_address:#x} expects an LDR instruction"
                        )));
                    }
                    insn = (insn & 0x001f_ffff) | 0x9100_0000;
                }
                let mut scale = 0u32;
                if insn & 0x3b00_0000 == 0x3900_0000 {
                    scale = insn >> 30;
                    if scale == 0 && insn & 0x0480_0000 == 0x0480_0000 {
                        scale = 4;
                    }
                }
                let low = destination & 0xfff;
                let mask = 1u64.checked_shl(scale).unwrap_or(1).saturating_sub(1);
                if low & mask != 0 {
                    return Err(Error::Limit(format!(
                        "PAGEOFF12 relocation at {place_address:#x}: target {destination:#x} is not aligned to the access size"
                    )));
                }
                let imm = u32::try_from(low >> scale).unwrap_or(0);
                put_insn(out, at, (insn & !0x003f_fc00) | (imm << 10))?;
            }
            ARM64_RELOC_POINTER_TO_GOT => {
                let slot = symbol.and_then(|id| resolve.got(id)).ok_or_else(|| {
                    Error::Internal(format!(
                        "POINTER_TO_GOT relocation at {place_address:#x} without a GOT slot"
                    ))
                })?;
                if decoded.pcrel && decoded.length == 2 {
                    let delta = slot.wrapping_sub(place_address) as i64;
                    let delta32 = i32::try_from(delta)
                        .map_err(|_| range_error("POINTER_TO_GOT", delta, place_address))?;
                    put32(out, at, delta32 as u32)?;
                } else if decoded.length == 3 {
                    put64(out, at, slot)?;
                    return Ok(Some(Fixup {
                        address: place_address,
                        kind: FixupKind::Rebase(slot),
                    }));
                } else {
                    return Err(Error::Limit(format!(
                        "POINTER_TO_GOT relocation at {place_address:#x} with an unsupported size"
                    )));
                }
            }
            _ => {
                return Err(Error::Internal(format!(
                    "unhandled arm64 relocation type {}",
                    decoded.r_type
                )));
            }
        }
        return Ok(None);
    }

    // x86_64 PC-relative fields.
    let destination = match decoded.r_type {
        X86_64_RELOC_BRANCH => match symbol.and_then(|id| resolve.stub(id)) {
            Some(stub) => stub.wrapping_add(addend as u64),
            None => address_of(resolve.value(target, addend)?, "BRANCH", place_address)?,
        },
        X86_64_RELOC_GOT_LOAD | X86_64_RELOC_GOT | X86_64_RELOC_TLV => {
            let slot = match (decoded.r_type, symbol) {
                (X86_64_RELOC_TLV, Some(id)) => resolve.tlv_pointer(id),
                (_, Some(id)) => resolve.got(id),
                _ => None,
            };
            match slot {
                Some(slot) => slot.wrapping_add(addend as u64),
                None if decoded.r_type != X86_64_RELOC_GOT => {
                    // Defined here: `movq foo@GOTPCREL(%rip)` becomes
                    // `leaq foo(%rip)`.
                    let opcode = at
                        .checked_sub(2)
                        .and_then(|i| out.get_mut(i))
                        .ok_or_else(|| Error::Internal("relocation outside its section".into()))?;
                    if *opcode != 0x8b {
                        return Err(Error::Limit(format!(
                            "relocation at {place_address:#x} expects a MOVQ instruction"
                        )));
                    }
                    *opcode = 0x8d;
                    address_of(resolve.value(target, addend)?, "GOT_LOAD", place_address)?
                }
                None => {
                    return Err(Error::Internal(format!(
                        "GOT relocation at {place_address:#x} without a slot"
                    )));
                }
            }
        }
        X86_64_RELOC_SIGNED
        | X86_64_RELOC_SIGNED_1
        | X86_64_RELOC_SIGNED_2
        | X86_64_RELOC_SIGNED_4 => {
            address_of(resolve.value(target, addend)?, "SIGNED", place_address)?
        }
        _ => {
            return Err(Error::Internal(format!(
                "unhandled x86_64 relocation type {}",
                decoded.r_type
            )));
        }
    };
    if decoded.length != 2 {
        return Err(Error::Limit(format!(
            "PC-relative relocation at {place_address:#x} with an unsupported size"
        )));
    }
    // The CPU adds the address after the instruction, which ends 1, 2 or 4
    // bytes after the field for `SIGNED_n`.
    let end = place_address
        .wrapping_add(4)
        .wrapping_add(pcrel_extra(decoded.r_type) as u64);
    let delta = destination.wrapping_sub(end) as i64;
    let delta32 =
        i32::try_from(delta).map_err(|_| range_error("PC-relative", delta, place_address))?;
    put32(out, at, delta32 as u32)?;
    Ok(None)
}

/// The destination of an arm64 page relocation: the symbol itself, its GOT
/// slot or its thread-local pointer slot.
fn indirect(
    r_type: u8,
    symbol: Option<SymbolId>,
    target: Place,
    addend: i64,
    resolve: &dyn Resolve,
    place_address: u64,
) -> Result<(u64, bool)> {
    let slot = match r_type {
        ARM64_RELOC_GOT_LOAD_PAGE21 | ARM64_RELOC_GOT_LOAD_PAGEOFF12 => {
            symbol.and_then(|id| resolve.got(id))
        }
        ARM64_RELOC_TLVP_LOAD_PAGE21 | ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => {
            symbol.and_then(|id| resolve.tlv_pointer(id))
        }
        _ => {
            return Ok((
                address_of(resolve.value(target, addend)?, "PAGE", place_address)?,
                false,
            ));
        }
    };
    match slot {
        Some(slot) => Ok((slot, false)),
        // No slot: the symbol is defined here, so the load is relaxed to
        // the symbol's address.
        None => Ok((
            address_of(resolve.value(target, addend)?, "GOT_LOAD", place_address)?,
            true,
        )),
    }
}

/// Whether a relocation type goes through the GOT, a stub or a TLV pointer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    /// A `__got` slot.
    pub got: bool,
    /// A stub (for imports).
    pub stub: bool,
    /// A `__thread_ptrs` slot.
    pub tlv: bool,
    /// The slot is needed even for a symbol defined in the image (the
    /// relocation cannot be relaxed to the symbol's address).
    pub pointer: bool,
}

/// What a relocation of type `r_type` needs for its symbol.
#[must_use]
pub fn needs(arm64: bool, r_type: u8) -> Needs {
    if arm64 {
        match r_type {
            ARM64_RELOC_GOT_LOAD_PAGE21 | ARM64_RELOC_GOT_LOAD_PAGEOFF12 => Needs {
                got: true,
                ..Needs::default()
            },
            ARM64_RELOC_POINTER_TO_GOT => Needs {
                got: true,
                pointer: true,
                ..Needs::default()
            },
            ARM64_RELOC_TLVP_LOAD_PAGE21 | ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => Needs {
                tlv: true,
                ..Needs::default()
            },
            ARM64_RELOC_BRANCH26 => Needs {
                stub: true,
                ..Needs::default()
            },
            _ => Needs::default(),
        }
    } else {
        match r_type {
            X86_64_RELOC_GOT_LOAD => Needs {
                got: true,
                ..Needs::default()
            },
            X86_64_RELOC_GOT => Needs {
                got: true,
                pointer: true,
                ..Needs::default()
            },
            X86_64_RELOC_TLV => Needs {
                tlv: true,
                ..Needs::default()
            },
            X86_64_RELOC_BRANCH => Needs {
                stub: true,
                ..Needs::default()
            },
            _ => Needs::default(),
        }
    }
}

/// Whether the relocation writes a 64-bit pointer (a candidate for a bind).
#[must_use]
pub fn is_pointer(arm64: bool, decoded: &Decoded) -> bool {
    decoded.subtrahend.is_none()
        && decoded.length == 3
        && ((arm64 && decoded.r_type == ARM64_RELOC_UNSIGNED)
            || (!arm64 && decoded.r_type == X86_64_RELOC_UNSIGNED))
}

/// Marker so unused-import lints stay quiet on some targets.
const _: u8 = X86_64_RELOC_SUBTRACTOR;
