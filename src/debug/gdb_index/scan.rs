//! Names from DIEs, for objects without `.debug_gnu_pubnames` and
//! `.debug_gnu_pubtypes` (lld indexes no names for them; qld reads them
//! from the DIEs, as gold does).
//!
//! The names are those Clang's `-ggnu-pubnames` would list, with the same
//! kinds and linkage (LLVM's `DwarfCompileUnit::addGlobalName`,
//! `addGlobalType` and `computeIndexValue`):
//!
//! - names: variable and function definitions (functions with code or an
//!   abstract inline definition, variables at namespace scope or with a
//!   static location), namespaces (`(anonymous namespace)` for unnamed
//!   ones), and the enumerators of enumerations at namespace scope;
//! - types: named, complete types at namespace scope, and the declarations
//!   that stand for types moved to type units (`DW_AT_signature`), named
//!   after the type unit's type.
//!
//! In C++ units a name is qualified by its enclosing namespaces, classes
//! and functions (a definition with `DW_AT_specification` takes its name
//! and scope from the declaration). Each qualified name is listed once per
//! table and unit, with the kind of its last DIE. The names and types of
//! the type units a unit refers to (directly or through other type units)
//! are added as types with external linkage, unless the unit lists them
//! itself.

use std::borrow::Cow;

use super::input::{DebugObject, Malformed, Reader, Section};
use super::names::{NameEntry, gdb_hash};
use super::unit::{self, AbbrevTable, Abbrevs, DW_AT_NAME, UnitHeader, UnitInfo, Value};

// Tags.
const DW_TAG_CLASS_TYPE: u64 = 0x02;
const DW_TAG_ENUMERATION_TYPE: u64 = 0x04;
const DW_TAG_LEXICAL_BLOCK: u64 = 0x0b;
const DW_TAG_STRUCTURE_TYPE: u64 = 0x13;
const DW_TAG_TYPEDEF: u64 = 0x16;
const DW_TAG_UNION_TYPE: u64 = 0x17;
const DW_TAG_SUBRANGE_TYPE: u64 = 0x21;
const DW_TAG_BASE_TYPE: u64 = 0x24;
const DW_TAG_ENUMERATOR: u64 = 0x28;
const DW_TAG_SUBPROGRAM: u64 = 0x2e;
const DW_TAG_VARIABLE: u64 = 0x34;
const DW_TAG_NAMESPACE: u64 = 0x39;
const DW_TAG_TEMPLATE_ALIAS: u64 = 0x4309;
/// Other type tags: listed with no kind when named.
const OTHER_TYPES: [u64; 17] = [
    0x01,   // array_type
    0x0f,   // pointer_type
    0x10,   // reference_type
    0x12,   // string_type
    0x15,   // subroutine_type
    0x1f,   // ptr_to_member_type
    0x20,   // set_type
    0x26,   // const_type
    0x2a,   // file_type
    0x35,   // volatile_type
    0x37,   // restrict_type
    0x3b,   // unspecified_type
    0x42,   // rvalue_reference_type
    0x44,   // fixed_point_type (LLVM's DIFixedPointType)
    0x47,   // atomic_type
    0x4b,   // immutable_type
    0x4101, // GNU template_template_param
];

// Attributes.
const DW_AT_LOCATION: u64 = 0x02;
const DW_AT_ABSTRACT_ORIGIN: u64 = 0x31;
const DW_AT_DECLARATION: u64 = 0x3c;
const DW_AT_EXTERNAL: u64 = 0x3f;
const DW_AT_SPECIFICATION: u64 = 0x47;
const DW_AT_SIGNATURE: u64 = 0x69;

// Location operations that give a static address.
const DW_OP_ADDR: u8 = 0x03;
const DW_OP_CONST4U: u8 = 0x0c;
const DW_OP_CONST8U: u8 = 0x0e;
const DW_OP_ADDRX: u8 = 0xa1;
const DW_OP_GNU_ADDR_INDEX: u8 = 0xfb;

// GDB index kinds.
const KIND_TYPE: u32 = 1;
const KIND_VARIABLE: u32 = 2;
const KIND_FUNCTION: u32 = 3;

/// Kind and linkage bits of names that come from type units: a type with
/// external linkage.
const TYPE_UNIT_BITS: u32 = KIND_TYPE << 4;

/// No DIE.
const NONE: u32 = u32::MAX;

/// What the scan keeps of a DIE.
#[derive(Clone, Copy)]
struct Die<'a> {
    offset: usize,
    parent: u32,
    tag: u64,
    name: Option<Value<'a>>,
    /// `DW_AT_specification`: offset in the section of the declaration.
    spec: Option<usize>,
    /// `DW_AT_signature`: the type unit that holds the type.
    signature: Option<u64>,
    abstract_origin: bool,
    declaration: bool,
    external: bool,
    static_location: bool,
}

/// The DIEs of a unit, and the type signatures it refers to.
struct Dies<'a> {
    list: Vec<Die<'a>>,
    signatures: Vec<u64>,
}

/// A type unit, as the units that refer to it see it.
#[derive(Default)]
struct TypeUnit<'a> {
    /// The name of its type.
    name: Option<&'a [u8]>,
    /// Its names (`false`) and types (`true`), qualified.
    names: Vec<(bool, Cow<'a, [u8]>)>,
    /// The signatures it refers to.
    refs: Vec<u64>,
}

/// The type units of an object, by signature.
#[derive(Default)]
pub(crate) struct TypeUnits<'a> {
    units: hashbrown::HashMap<u64, TypeUnit<'a>, foldhash::fast::FixedState>,
}

impl<'a> TypeUnits<'a> {
    /// Reads the type units of `obj`. Malformed units are left out, with
    /// the problem reported.
    pub(crate) fn read(
        obj: &DebugObject<'_, 'a>,
        abbrevs: &mut Abbrevs,
        problems: &mut Vec<(u32, Malformed)>,
    ) -> Self {
        let mut this = Self::default();
        // (unit header, section, its DIEs, unit DIE) of every type unit.
        let mut read: Vec<(UnitHeader, &Section<'a>, Dies<'a>, UnitInfo)> = Vec::new();
        for (section, types) in &obj.type_units {
            let (headers, error) = unit::unit_headers(obj, section, *types);
            if let Some(error) = error {
                problems.push((section.index, error));
            }
            for header in headers.into_iter().filter(UnitHeader::is_type_unit) {
                let result = abbrevs.get(obj, &header).and_then(|table| {
                    let Some(info) = unit::unit_info(obj, section, &header, table)? else {
                        return Ok(None);
                    };
                    let dies = if info.children {
                        read_dies(obj, section, &header, table, info.children_at)?
                    } else {
                        Dies {
                            list: Vec::new(),
                            signatures: Vec::new(),
                        }
                    };
                    Ok(Some((dies, info)))
                });
                match result {
                    Ok(Some((dies, info))) => read.push((header, section, dies, info)),
                    Ok(None) => {}
                    Err(e) => problems.push((section.index, e)),
                }
            }
        }
        // Type names first: types in one unit name those of others.
        let empty = Self::default();
        let mut type_names = Vec::with_capacity(read.len());
        for (header, _, dies, info) in &read {
            let scan = Scan {
                obj,
                unit: header,
                bases: info.bases,
                dies: &dies.list,
                cplusplus: info.language.is_some_and(is_cplusplus),
                type_units: &empty,
            };
            let type_die = u64::try_from(header.offset)
                .ok()
                .and_then(|o| o.checked_add(header.type_offset))
                .and_then(|o| usize::try_from(o).ok())
                .and_then(|o| scan.at_offset(o))
                .and_then(|i| scan.die(i));
            type_names.push((header.signature, type_die.and_then(|d| scan.string(d.name))));
        }
        for (signature, name) in type_names {
            this.units.insert(
                signature,
                TypeUnit {
                    name,
                    ..TypeUnit::default()
                },
            );
        }
        let mut lists = Vec::with_capacity(read.len());
        for (header, _, dies, info) in &read {
            let scan = Scan {
                obj,
                unit: header,
                bases: info.bases,
                dies: &dies.list,
                cplusplus: info.language.is_some_and(is_cplusplus),
                type_units: &this,
            };
            let mut names = Vec::new();
            for (i, d) in dies.list.iter().enumerate() {
                let i = u32::try_from(i).unwrap_or(NONE);
                if let Some((name, _)) = scan.global_name(i, d) {
                    names.push((false, name));
                }
                if let Some((name, _)) = scan.global_type(i, d) {
                    names.push((true, name));
                }
            }
            lists.push((header.signature, names, dies.signatures.clone()));
        }
        for (signature, names, refs) in lists {
            if let Some(unit) = this.units.get_mut(&signature) {
                unit.names = names;
                unit.refs = refs;
            }
        }
        this
    }

    fn name(&self, signature: u64) -> Option<&'a [u8]> {
        self.units.get(&signature).and_then(|u| u.name)
    }

    /// The type units reachable from `signatures`, in the order first
    /// reached.
    fn closure(&self, signatures: &[u64]) -> Vec<&TypeUnit<'a>> {
        let mut seen: hashbrown::HashSet<u64, foldhash::fast::FixedState> =
            hashbrown::HashSet::with_hasher(foldhash::fast::FixedState::default());
        let mut queue: Vec<u64> = signatures.to_vec();
        let mut out = Vec::new();
        let mut next = 0usize;
        while let Some(&signature) = queue.get(next) {
            next = next.saturating_add(1);
            if !seen.insert(signature) {
                continue;
            }
            if let Some(unit) = self.units.get(&signature) {
                queue.extend_from_slice(&unit.refs);
                out.push(unit);
            }
        }
        out
    }
}

/// Adds the names of unit `unit` (index `index` in its object) to `out`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn names<'a>(
    obj: &DebugObject<'_, 'a>,
    info: &Section<'a>,
    unit: &UnitHeader,
    abbrevs: &AbbrevTable,
    die: &UnitInfo,
    index: u32,
    type_units: &TypeUnits<'a>,
    out: &mut Vec<NameEntry<'a>>,
) -> Result<(), Malformed> {
    if !die.children {
        return Ok(());
    }
    let dies = read_dies(obj, info, unit, abbrevs, die.children_at)?;
    let scan = Scan {
        obj,
        unit,
        bases: die.bases,
        dies: &dies.list,
        cplusplus: die.language.is_some_and(is_cplusplus),
        type_units,
    };
    // (full name, kind and linkage bits, DIE offset); later DIEs replace
    // earlier ones of the same name.
    let mut names: Vec<Listed<'a>> = Vec::new();
    let mut types: Vec<Listed<'a>> = Vec::new();
    for (i, d) in dies.list.iter().enumerate() {
        let i = u32::try_from(i).unwrap_or(NONE);
        if let Some((name, bits)) = scan.global_name(i, d) {
            names.push((name, bits, d.offset));
        }
        if let Some((name, bits)) = scan.global_type(i, d) {
            types.push((name, bits, d.offset));
        }
    }
    let mut names = dedup(names);
    let mut types = dedup(types);
    // Names of type units the unit refers to, if not its own. They carry
    // the unit DIE, which sorts first.
    let mut from_type_units: [Vec<(Cow<'a, [u8]>, u32)>; 2] = [Vec::new(), Vec::new()];
    for tu in type_units.closure(&dies.signatures) {
        for (is_type, name) in &tu.names {
            let (own, extra) = if *is_type {
                (&types, &mut from_type_units[1])
            } else {
                (&names, &mut from_type_units[0])
            };
            if !own.iter().any(|(n, _)| n == name) && !extra.iter().any(|(n, _)| n == name) {
                extra.push((name.clone(), TYPE_UNIT_BITS));
            }
        }
    }
    let [tu_names, tu_types] = from_type_units;
    names.splice(0..0, tu_names);
    types.splice(0..0, tu_types);
    for (name, bits) in names.into_iter().chain(types) {
        let hash = gdb_hash(&name);
        out.push(NameEntry {
            name,
            hash,
            value: (bits << 24) | (index & 0x00ff_ffff),
        });
    }
    Ok(())
}

/// A name with its kind and linkage bits and its DIE's offset.
type Listed<'a> = (Cow<'a, [u8]>, u32, usize);

/// Keeps one entry per name (the last DIE's kind), ordered by the offset
/// of that DIE, as Clang emits them.
fn dedup<'a>(list: Vec<Listed<'a>>) -> Vec<(Cow<'a, [u8]>, u32)> {
    let mut seen: hashbrown::HashMap<Cow<'a, [u8]>, usize, foldhash::fast::FixedState> =
        hashbrown::HashMap::with_hasher(foldhash::fast::FixedState::default());
    let mut keep: Vec<Option<Listed<'a>>> = Vec::with_capacity(list.len());
    for entry in list {
        if let Some(&at) = seen.get(&entry.0)
            && let Some(slot) = keep.get_mut(at)
        {
            *slot = None;
        }
        seen.insert(entry.0.clone(), keep.len());
        keep.push(Some(entry));
    }
    let mut kept: Vec<Listed<'a>> = keep.into_iter().flatten().collect();
    kept.sort_by_key(|&(_, _, offset)| offset);
    kept.into_iter().map(|(n, b, _)| (n, b)).collect()
}

/// Whether `DW_AT_language` is a C++ dialect (LLVM's `isCPlusPlus`).
fn is_cplusplus(language: u64) -> bool {
    matches!(language, 0x04 | 0x19 | 0x1a | 0x21 | 0x2a | 0x2b)
}

/// Reads the DIEs of a unit after the unit DIE.
fn read_dies<'a>(
    obj: &DebugObject<'_, 'a>,
    info: &Section<'a>,
    unit: &UnitHeader,
    abbrevs: &AbbrevTable,
    start: usize,
) -> Result<Dies<'a>, Malformed> {
    let data = info.data.get(..unit.end).unwrap_or_default();
    let mut r = Reader::at(data, start);
    let mut dies: Vec<Die<'a>> = Vec::new();
    let mut signatures = Vec::new();
    // Parents of the DIEs being read; the unit DIE is NONE.
    let mut stack: Vec<u32> = vec![NONE];
    while !r.is_empty() {
        let offset = r.pos();
        let code = r.uleb()?;
        if code == 0 {
            stack.pop();
            if stack.is_empty() {
                break;
            }
            continue;
        }
        let abbrev = abbrevs
            .get(code)
            .ok_or_else(|| r.error("abbreviation code (not found)"))?;
        let mut die = Die {
            offset,
            parent: stack.last().copied().unwrap_or(NONE),
            tag: abbrev.tag,
            name: None,
            spec: None,
            signature: None,
            abstract_origin: false,
            declaration: false,
            external: false,
            static_location: false,
        };
        for &(at, form, implicit) in &abbrev.attrs {
            match at {
                DW_AT_NAME | DW_AT_SPECIFICATION | DW_AT_LOCATION | DW_AT_SIGNATURE => {
                    let value = unit::read_value(obj, info, &mut r, form, implicit, unit)?;
                    match (at, value) {
                        (DW_AT_NAME, _) => die.name = Some(value),
                        (DW_AT_SPECIFICATION, Value::Ref(o)) => {
                            die.spec = usize::try_from(o)
                                .ok()
                                .and_then(|o| unit.offset.checked_add(o));
                        }
                        (DW_AT_SPECIFICATION, Value::RefAddr(o)) => {
                            die.spec = usize::try_from(o).ok();
                        }
                        (DW_AT_SIGNATURE, Value::Signature(s)) => {
                            die.signature = Some(s);
                            signatures.push(s);
                        }
                        (DW_AT_LOCATION, Value::Block(block)) => {
                            die.static_location = matches!(
                                block.first(),
                                Some(
                                    &(DW_OP_ADDR
                                        | DW_OP_ADDRX
                                        | DW_OP_GNU_ADDR_INDEX
                                        | DW_OP_CONST4U
                                        | DW_OP_CONST8U)
                                )
                            );
                        }
                        _ => {}
                    }
                }
                DW_AT_ABSTRACT_ORIGIN => {
                    die.abstract_origin = true;
                    unit::skip_value(&mut r, form, unit)?;
                }
                DW_AT_DECLARATION | DW_AT_EXTERNAL => {
                    let value = unit::read_value(obj, info, &mut r, form, implicit, unit)?;
                    let set = value.unsigned().is_some_and(|v| v != 0);
                    if at == DW_AT_DECLARATION {
                        die.declaration = set;
                    } else {
                        die.external = set;
                    }
                }
                _ if form == unit::DW_FORM_REF_SIG8 => signatures.push(r.uint(8)?),
                _ => unit::skip_value(&mut r, form, unit)?,
            }
        }
        let index = u32::try_from(dies.len()).map_err(|_| r.error("too many DIEs"))?;
        dies.push(die);
        if abbrev.children {
            stack.push(index);
        }
    }
    Ok(Dies {
        list: dies,
        signatures,
    })
}

struct Scan<'s, 'o, 'a> {
    obj: &'s DebugObject<'o, 'a>,
    unit: &'s UnitHeader,
    bases: unit::Bases,
    dies: &'s [Die<'a>],
    cplusplus: bool,
    type_units: &'s TypeUnits<'a>,
}

impl<'a> Scan<'_, '_, 'a> {
    fn die(&self, index: u32) -> Option<&Die<'a>> {
        self.dies.get(usize::try_from(index).ok()?)
    }

    fn at_offset(&self, offset: usize) -> Option<u32> {
        let at = self.dies.binary_search_by_key(&offset, |d| d.offset).ok()?;
        u32::try_from(at).ok()
    }

    fn string(&self, value: Option<Value<'a>>) -> Option<&'a [u8]> {
        unit::string(self.obj, value?, self.unit, &self.bases)
    }

    /// The declaration a definition refers to, if any.
    fn spec(&self, die: &Die<'a>) -> Option<(u32, &Die<'a>)> {
        let at = self.at_offset(die.spec?)?;
        Some((at, self.die(at)?))
    }

    /// The name of a DIE: its own, its declaration's, or its type unit's.
    fn name_of(&self, die: &Die<'a>) -> Option<&'a [u8]> {
        self.string(die.name)
            .or_else(|| self.spec(die).and_then(|(_, s)| self.string(s.name)))
            .or_else(|| self.type_units.name(die.signature?))
    }

    /// The scope a DIE's name is qualified by: its declaration's parent
    /// for definitions with one, its parent otherwise.
    fn scope_of(&self, die: &Die<'a>) -> u32 {
        match self.spec(die) {
            Some((_, spec)) => spec.parent,
            None => die.parent,
        }
    }

    /// `Scope::` prefixes for C++ (LLVM's `getParentContextString`).
    fn prefix(&self, mut scope: u32) -> Vec<u8> {
        if !self.cplusplus {
            return Vec::new();
        }
        let mut parts: Vec<&[u8]> = Vec::new();
        // Bounded: a malformed chain of specifications cannot loop.
        for _ in 0..self.dies.len() {
            let Some(die) = self.die(scope) else { break };
            let name = match self.name_of(die) {
                Some(name) if !name.is_empty() => Some(name),
                _ if die.tag == DW_TAG_NAMESPACE => Some(&b"(anonymous namespace)"[..]),
                _ => None,
            };
            if let Some(name) = name {
                parts.push(name);
            }
            scope = self.scope_of(die);
        }
        let mut out = Vec::new();
        for part in parts.iter().rev() {
            out.extend_from_slice(part);
            out.extend_from_slice(b"::");
        }
        out
    }

    fn qualified(&self, scope: u32, name: &'a [u8]) -> Cow<'a, [u8]> {
        let mut prefix = self.prefix(scope);
        if prefix.is_empty() {
            return Cow::Borrowed(name);
        }
        prefix.extend_from_slice(name);
        Cow::Owned(prefix)
    }

    /// Whether the DIE at `index` sits in a namespace or at the top level.
    fn at_namespace_scope(&self, index: u32) -> bool {
        self.die(index).is_none_or(|d| d.tag == DW_TAG_NAMESPACE)
    }

    /// Whether `die` is inside a function (or a block of one).
    fn in_function(&self, die: &Die<'a>) -> bool {
        let mut scope = die.parent;
        for _ in 0..self.dies.len() {
            let Some(parent) = self.die(scope) else {
                return false;
            };
            match parent.tag {
                DW_TAG_SUBPROGRAM | DW_TAG_LEXICAL_BLOCK => return true,
                DW_TAG_NAMESPACE => scope = parent.parent,
                _ => return false,
            }
        }
        false
    }

    /// The linkage bit: external if the DIE (or its declaration) says so.
    fn linkage(&self, die: &Die<'a>) -> u32 {
        let external = match self.spec(die) {
            Some((_, spec)) => spec.external,
            None => die.external,
        };
        u32::from(!external)
    }

    /// A name for the names table.
    fn global_name(&self, _index: u32, die: &Die<'a>) -> Option<(Cow<'a, [u8]>, u32)> {
        let bits = |kind: u32, is_static: u32| (kind << 4) | (is_static << 7);
        match die.tag {
            DW_TAG_SUBPROGRAM => {
                if die.declaration || die.abstract_origin {
                    return None;
                }
                // Unnamed entities are listed with an empty name.
                let name = self.name_of(die).unwrap_or_default();
                Some((
                    self.qualified(self.scope_of(die), name),
                    bits(KIND_FUNCTION, self.linkage(die)),
                ))
            }
            DW_TAG_VARIABLE => {
                if die.declaration {
                    return None;
                }
                let global = if die.spec.is_some() {
                    true
                } else if self.in_function(die) {
                    die.static_location
                } else {
                    self.at_namespace_scope(die.parent)
                };
                if !global {
                    return None;
                }
                let name = self.name_of(die).unwrap_or_default();
                Some((
                    self.qualified(self.scope_of(die), name),
                    bits(KIND_VARIABLE, self.linkage(die)),
                ))
            }
            DW_TAG_NAMESPACE => {
                let name = match self.string(die.name) {
                    Some(name) if !name.is_empty() => name,
                    _ => b"(anonymous namespace)",
                };
                Some((self.qualified(die.parent, name), bits(KIND_TYPE, 0)))
            }
            DW_TAG_ENUMERATOR => {
                let enumeration = self.die(die.parent)?;
                if enumeration.tag != DW_TAG_ENUMERATION_TYPE
                    || !self.at_namespace_scope(enumeration.parent)
                {
                    return None;
                }
                let name = self.string(die.name)?;
                Some((
                    self.qualified(enumeration.parent, name),
                    bits(KIND_VARIABLE, 1),
                ))
            }
            _ => None,
        }
    }

    /// A name for the types table.
    fn global_type(&self, _index: u32, die: &Die<'a>) -> Option<(Cow<'a, [u8]>, u32)> {
        let bits = |kind: u32, is_static: u32| (kind << 4) | (is_static << 7);
        let kind_bits = match die.tag {
            DW_TAG_CLASS_TYPE
            | DW_TAG_STRUCTURE_TYPE
            | DW_TAG_UNION_TYPE
            | DW_TAG_ENUMERATION_TYPE => bits(KIND_TYPE, u32::from(!self.cplusplus)),
            DW_TAG_TYPEDEF | DW_TAG_BASE_TYPE | DW_TAG_SUBRANGE_TYPE | DW_TAG_TEMPLATE_ALIAS => {
                bits(KIND_TYPE, 1)
            }
            tag if OTHER_TYPES.contains(&tag) => bits(0, 0),
            _ => return None,
        };
        if !self.at_namespace_scope(die.parent) {
            return None;
        }
        // A declaration that stands for a type in a type unit is listed
        // under the type's name, even an empty one.
        if let Some(signature) = die.signature {
            let name = self.type_units.name(signature).unwrap_or_default();
            return Some((self.qualified(die.parent, name), kind_bits));
        }
        if die.declaration {
            return None;
        }
        let name = self.string(die.name).filter(|n| !n.is_empty())?;
        // Clang makes its array index type and the base types of typed
        // DWARF expressions (`DW_ATE_unsigned_8`, ...) without index entries.
        if die.tag == DW_TAG_BASE_TYPE
            && (name == b"__ARRAY_SIZE_TYPE__" || name.starts_with(b"DW_ATE_"))
        {
            return None;
        }
        Some((self.qualified(die.parent, name), kind_bits))
    }
}
