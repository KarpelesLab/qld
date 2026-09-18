//! Objective-C metadata the linker rewrites, as ld64 and lld do:
//!
//! - **Relative method lists** (`-objc_relative_method_lists`, the default
//!   from macOS 11, iOS 14, tvOS 14, watchOS 7): every class and category
//!   method list (`__OBJC_$_INSTANCE_METHODS_…`, `__OBJC_$_CLASS_METHODS_…`,
//!   `__OBJC_$_CATEGORY_INSTANCE_METHODS_…`,
//!   `__OBJC_$_CATEGORY_CLASS_METHODS_…`) is rewritten from three pointers
//!   per method to three 32-bit offsets (`entsize` 12 with flag
//!   `0x80000000`) and moved to `__TEXT,__objc_methlist`, where it needs no
//!   fixups. The name offset points to the selector's reference in
//!   `__objc_selrefs`, the type and implementation offsets to the type
//!   string and the function. Names without a selector reference get one
//!   from the generated object of [`objc_stubs`](super::objc_stubs)
//!   ([`missing_selrefs`], one extra link attempt).
//! - **Category merging** (`-objc_category_merging`, off by default as in
//!   lld): the categories of a class defined in the image are merged into
//!   the class (its `class_ro_t` and the metaclass's get combined method,
//!   protocol and property lists, category entries first), and several
//!   categories of a class defined elsewhere are merged into the first of
//!   them. Categories with `+load` (in `__objc_nlcatlist`) are left alone.
//!   The merged lists reuse the space of a list they replace; the merged
//!   categories, their other lists and their `__objc_catlist` entries are
//!   dropped.
//!
//! Both work on atoms after dead stripping: a [`Plan`] records, for each
//! rewritten atom, its new contents and the pointers or offsets the writer
//! fills in ([`sections`](super::sections)), and the layout gives it its
//! new size ([`layout`](super::layout)). Anything that does not look like
//! what clang emits (a list sharing its atom with other data, an unknown
//! entry size, a Swift class) is left as it is.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::{HashMap, HashSet};
use rayon::prelude::*;

use crate::error::Result;
use crate::ids::SymbolId;

use super::buf::{get32, get64, to_u64, to_usize};
use super::layout::atom_location;
use super::reloc::{self, Place};
use super::state::Link;

/// Relative method list flag in `entsize`.
pub const RELATIVE_METHODS: u32 = 0x8000_0000;
/// The `entsize` bits of a list header.
const ENTSIZE_MASK: u32 = 0x0000_fffc;
/// The flag bits of a method list header.
const FLAGS_MASK: u32 = 0xffff_0003;

/// Symbol prefixes of the method lists that become relative, as in lld.
const METHOD_LIST_PREFIXES: [&[u8]; 4] = [
    b"__OBJC_$_INSTANCE_METHODS_",
    b"__OBJC_$_CLASS_METHODS_",
    b"__OBJC_$_CATEGORY_INSTANCE_METHODS_",
    b"__OBJC_$_CATEGORY_CLASS_METHODS_",
];

/// What a rewritten pointer or offset refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// The place.
    pub place: Place,
    /// The addend, for a [`Place::Symbol`] (other places fold it in).
    pub addend: i64,
}

/// How a [`Field`] is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// A 64-bit pointer (with a rebase or bind).
    Pointer,
    /// A 32-bit offset from the field to the target.
    Relative,
}

/// One value the writer fills into a rewritten atom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    /// Offset within the atom.
    pub offset: u64,
    /// How it is written.
    pub kind: FieldKind,
    /// What it refers to.
    pub target: Target,
}

/// The new contents of an atom.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rewrite {
    /// The bytes, with zeros where [`Rewrite::fields`] go.
    pub bytes: Vec<u8>,
    /// Alignment, as a power of two.
    pub align: u32,
    /// Pointers and offsets to fill in.
    pub fields: Vec<Field>,
    /// A relative method list: placed in `__TEXT,__objc_methlist`.
    pub method_list: bool,
}

/// The rewritten atoms of a link.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// Rewrites by global atom index.
    pub rewrites: HashMap<usize, Rewrite>,
}

impl Plan {
    /// The rewrite of global atom `atom`, if any.
    #[must_use]
    pub fn rewrite(&self, atom: usize) -> Option<&Rewrite> {
        if self.rewrites.is_empty() {
            return None;
        }
        self.rewrites.get(&atom)
    }
}

/// The pointers of the Objective-C metadata atoms: for each atom, its
/// pointer fields by offset.
struct Pointers {
    map: HashMap<usize, Vec<(u64, Target)>>,
}

impl Pointers {
    fn at(&self, atom: usize, offset: u64) -> Option<Target> {
        let list = self.map.get(&atom)?;
        list.binary_search_by_key(&offset, |&(o, _)| o)
            .ok()
            .and_then(|i| list.get(i))
            .map(|&(_, t)| t)
    }

    fn all(&self, atom: usize) -> &[(u64, Target)] {
        self.map.get(&atom).map_or(&[], Vec::as_slice)
    }
}

/// Whether an input section holds metadata this module reads.
fn is_metadata(segname: &[u8], sectname: &[u8]) -> bool {
    matches!(segname, b"__DATA" | b"__DATA_CONST")
        && matches!(
            sectname,
            b"__objc_const"
                | b"__objc_data"
                | b"__objc_catlist"
                | b"__objc_nlcatlist"
                | b"__objc_selrefs"
        )
}

/// Collects the pointer fields of the metadata atoms, of live atoms only
/// or (before dead stripping) of every atom of the live files.
fn pointers(link: &Link<'_>, live_only: bool) -> Result<Pointers> {
    let per_file: Vec<Result<Vec<(usize, u64, Target)>>> = (0..link.files.len())
        .into_par_iter()
        .map(|file| {
            let mut out = Vec::new();
            let Some(object) = link.object(file) else {
                return Ok(out);
            };
            let arm64 = link.config.is_arm64();
            for (section_index, relocations) in object.relocations.iter().enumerate() {
                let Some(section) = object.file.sections().get(section_index) else {
                    continue;
                };
                if !is_metadata(section.segname, section.sectname) {
                    continue;
                }
                let data = object.file.section_data(section_index)?;
                for relocation in relocations {
                    if live_only && !link.is_live(file, relocation.atom) {
                        continue;
                    }
                    let decoded = reloc::decode(
                        link,
                        file,
                        object,
                        section_index,
                        data,
                        &relocation.relocation,
                    )?;
                    if !reloc::is_pointer(arm64, &decoded) {
                        continue;
                    }
                    let Ok(place) =
                        reloc::place(link, file, object, decoded.referent, decoded.addend)
                    else {
                        continue;
                    };
                    let addend = match place {
                        Place::Symbol(_) => decoded.addend,
                        _ => 0,
                    };
                    let Some(atom) = object.atoms.atoms().get(relocation.atom) else {
                        continue;
                    };
                    let within = decoded.offset.saturating_sub(atom.offset);
                    out.push((
                        link.atom_id(file, relocation.atom),
                        within,
                        Target { place, addend },
                    ));
                }
            }
            Ok(out)
        })
        .collect();
    let mut map: HashMap<usize, Vec<(u64, Target)>> = HashMap::new();
    for result in per_file {
        for (atom, offset, target) in result? {
            map.entry(atom).or_default().push((offset, target));
        }
    }
    for list in map.values_mut() {
        list.sort_by_key(|&(offset, _)| offset);
        list.dedup_by_key(|&mut (offset, _)| offset);
    }
    Ok(Pointers { map })
}

/// The bytes of global atom `atom`.
fn atom_bytes<'a>(link: &'a Link<'_>, atom: usize) -> Option<&'a [u8]> {
    let (file, local) = atom_location(link, atom);
    let object = link.object(file)?;
    let info = object.atoms.atoms().get(local)?;
    let data = object
        .file
        .section_data(usize::try_from(info.section).ok()?)
        .ok()?;
    data.get(to_usize(info.offset)..to_usize(info.range().end))
}

/// Whether global atom `atom` carries a symbol whose name starts with one
/// of `prefixes`.
fn has_symbol_prefix(link: &Link<'_>, atom: usize, prefixes: &[&[u8]]) -> bool {
    let (file, local) = atom_location(link, atom);
    let Some(object) = link.object(file) else {
        return false;
    };
    object.atoms.atom_symbols(local).iter().any(|&symbol| {
        object
            .file
            .symbols()
            .get(symbol)
            .is_ok_and(|s| prefixes.iter().any(|p| s.name.starts_with(p)))
    })
}

/// The name of the section of global atom `atom`.
fn atom_section_name<'a>(link: &'a Link<'_>, atom: usize) -> Option<&'a [u8]> {
    let (file, local) = atom_location(link, atom);
    let object = link.object(file)?;
    let info = object.atoms.atoms().get(local)?;
    let section = object
        .file
        .sections()
        .get(usize::try_from(info.section).ok()?)?;
    Some(section.sectname)
}

/// The C string a target points to.
fn string_at(link: &Link<'_>, target: Target) -> Option<Vec<u8>> {
    let Place::Atom { atom, offset } = target.place else {
        return None;
    };
    let bytes = atom_bytes(link, atom)?;
    let rest = bytes.get(usize::try_from(offset).ok()?..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    rest.get(..end).map(<[u8]>::to_vec)
}

/// The atom a pointer designates, when it points to the start of one.
fn atom_start(target: Target) -> Option<usize> {
    match target.place {
        Place::Atom { atom, offset: 0 } => Some(atom),
        _ => None,
    }
}

/// One method: name, types, implementation; and the name's text.
#[derive(Clone, Debug)]
struct Method {
    name: Option<Target>,
    types: Option<Target>,
    imp: Option<Target>,
    selector: Option<Vec<u8>>,
}

/// A list of methods, properties or protocols, as the linker rebuilds it.
#[derive(Clone, Debug)]
enum List {
    /// `method_list_t`: header flags and methods.
    Methods { flags: u32, methods: Vec<Method> },
    /// `property_list_t`: (name, attributes) pairs.
    Properties(Vec<[Option<Target>; 2]>),
    /// `protocol_list_t`: protocol pointers.
    Protocols(Vec<Option<Target>>),
}

impl List {
    fn len(&self) -> usize {
        match self {
            Self::Methods { methods, .. } => methods.len(),
            Self::Properties(entries) => entries.len(),
            Self::Protocols(entries) => entries.len(),
        }
    }

    fn append(&mut self, other: &Self) -> bool {
        match (self, other) {
            (Self::Methods { methods, .. }, Self::Methods { methods: more, .. }) => {
                methods.extend(more.iter().cloned());
            }
            (Self::Properties(entries), Self::Properties(more)) => {
                entries.extend(more.iter().copied());
            }
            (Self::Protocols(entries), Self::Protocols(more)) => {
                entries.extend(more.iter().copied());
            }
            _ => return false,
        }
        true
    }
}

/// Which kind of list a field holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListKind {
    Methods,
    Properties,
    Protocols,
}

/// Parses the list in global atom `atom`, which must be exactly one list.
fn parse_list(link: &Link<'_>, pointers: &Pointers, atom: usize, kind: ListKind) -> Option<List> {
    let bytes = atom_bytes(link, atom)?;
    let ptr = |offset: u64| pointers.at(atom, offset);
    match kind {
        ListKind::Methods | ListKind::Properties => {
            let header = get32(bytes, 0)?;
            let count = get32(bytes, 4)?;
            if header & RELATIVE_METHODS != 0 {
                return None;
            }
            let entsize = header & ENTSIZE_MASK;
            let expected = if kind == ListKind::Methods { 24 } else { 16 };
            if entsize != expected {
                return None;
            }
            let size = u64::from(count)
                .checked_mul(u64::from(entsize))?
                .checked_add(8)?;
            if size != to_u64(bytes.len()) {
                return None;
            }
            if kind == ListKind::Methods {
                let mut methods = Vec::with_capacity(to_usize(u64::from(count)));
                for index in 0..u64::from(count) {
                    let base = index.checked_mul(24)?.checked_add(8)?;
                    let name = ptr(base);
                    let selector = name.and_then(|n| string_at(link, n));
                    methods.push(Method {
                        name,
                        types: ptr(base.checked_add(8)?),
                        imp: ptr(base.checked_add(16)?),
                        selector,
                    });
                }
                Some(List::Methods {
                    flags: header & FLAGS_MASK,
                    methods,
                })
            } else {
                let mut entries = Vec::with_capacity(to_usize(u64::from(count)));
                for index in 0..u64::from(count) {
                    let base = index.checked_mul(16)?.checked_add(8)?;
                    entries.push([ptr(base), ptr(base.checked_add(8)?)]);
                }
                Some(List::Properties(entries))
            }
        }
        ListKind::Protocols => {
            let count = get64(bytes, 0)?;
            // The count, the pointers and a null terminator.
            let size = count.checked_add(2)?.checked_mul(8)?;
            if size != to_u64(bytes.len()) {
                return None;
            }
            let mut entries = Vec::with_capacity(to_usize(count));
            for index in 0..count {
                entries.push(ptr(index.checked_add(1)?.checked_mul(8)?));
            }
            Some(List::Protocols(entries))
        }
    }
}

/// The selector references of the image, by selector name: the first
/// reference to each name, in input order.
fn selrefs(link: &Link<'_>, pointers: &Pointers, live_only: bool) -> HashMap<Vec<u8>, usize> {
    let mut out = HashMap::new();
    for file in 0..link.files.len() {
        let Some(object) = link.object(file) else {
            continue;
        };
        for (section_index, section) in object.file.sections().iter().enumerate() {
            if section.sectname != b"__objc_selrefs" {
                continue;
            }
            let Some(range) = object.atoms.section_range(section_index) else {
                continue;
            };
            for local in range {
                if live_only && !link.is_live(file, local) {
                    continue;
                }
                let atom = link.atom_id(file, local);
                let Some(bytes) = atom_bytes(link, atom) else {
                    continue;
                };
                if bytes.len() != 8 {
                    continue;
                }
                if let Some(name) = pointers.at(atom, 0).and_then(|t| string_at(link, t)) {
                    out.entry(name).or_insert(atom);
                }
            }
        }
    }
    out
}

/// The method list atoms that become relative.
fn method_list_atoms(link: &Link<'_>, live_only: bool) -> Vec<usize> {
    let mut out = Vec::new();
    for file in 0..link.files.len() {
        let Some(object) = link.object(file) else {
            continue;
        };
        for (section_index, section) in object.file.sections().iter().enumerate() {
            if section.sectname != b"__objc_const" {
                continue;
            }
            let Some(range) = object.atoms.section_range(section_index) else {
                continue;
            };
            for local in range {
                if live_only && !link.is_live(file, local) {
                    continue;
                }
                let atom = link.atom_id(file, local);
                if has_symbol_prefix(link, atom, &METHOD_LIST_PREFIXES) {
                    out.push(atom);
                }
            }
        }
    }
    out
}

/// The selector names relative method lists need that no selector
/// reference of the link names, sorted. Runs before dead stripping, on
/// every method list of the live files.
///
/// # Errors
///
/// Malformed relocations.
pub fn missing_selrefs(link: &Link<'_>) -> Result<Vec<Vec<u8>>> {
    if !link.config.relative_method_lists {
        return Ok(Vec::new());
    }
    let lists = method_list_atoms(link, false);
    if lists.is_empty() {
        return Ok(Vec::new());
    }
    let pointers = pointers(link, false)?;
    let known = selrefs(link, &pointers, false);
    let mut missing = Vec::new();
    for atom in lists {
        let Some(List::Methods { methods, .. }) =
            parse_list(link, &pointers, atom, ListKind::Methods)
        else {
            continue;
        };
        for method in methods {
            if let Some(name) = method.selector
                && !known.contains_key(&name)
            {
                missing.push(name);
            }
        }
    }
    missing.sort();
    missing.dedup();
    Ok(missing)
}

/// A category to merge: its catlist slot, its body and its lists.
struct Category {
    /// The `__objc_catlist` atom and the slot's offset in it.
    slot: (usize, u64),
    /// The `category_t` atom.
    body: usize,
}

/// `category_t` field offsets.
const CAT_INSTANCE_METHODS: u64 = 16;
const CAT_CLASS_METHODS: u64 = 24;
const CAT_PROTOCOLS: u64 = 32;
const CAT_INSTANCE_PROPS: u64 = 40;
const CAT_CLASS_PROPS: u64 = 48;
/// `class_t` field offsets.
const CLASS_ISA: u64 = 0;
const CLASS_DATA: u64 = 32;
/// `class_ro_t` field offsets and size.
const RO_METHODS: u64 = 32;
const RO_PROTOCOLS: u64 = 40;
const RO_PROPERTIES: u64 = 64;
const RO_SIZE: usize = 72;

/// Builds the plan for `link` (after dead stripping) and drops the atoms
/// merging makes unused.
///
/// # Errors
///
/// Malformed relocations.
pub fn plan(link: &mut Link<'_>) -> Result<()> {
    let config = link.config;
    if config.is_relocatable() || !(config.relative_method_lists || config.objc_category_merging) {
        return Ok(());
    }
    let (plan, dead) = build(link)?;
    for atom in dead {
        if let Some(slot) = link.live.get_mut(atom) {
            *slot = false;
        }
    }
    link.objc = plan;
    Ok(())
}

/// The final contents of the lists and containers merging changes.
#[derive(Default)]
struct Merged {
    /// New list contents, by the atom that holds them.
    lists: HashMap<usize, List>,
    /// Containers (`class_ro_t`, `category_t`) whose list fields change:
    /// field offset → list atom (or `None` for an empty list).
    containers: HashMap<usize, Vec<(u64, Option<usize>)>>,
    /// Merged categories' names (`First|Second`, as lld names them), stored
    /// after the `category_t`.
    names: HashMap<usize, Vec<u8>>,
    /// `__objc_catlist` atoms with the slots that stay.
    catlists: HashMap<usize, Vec<u64>>,
    /// Atoms no longer used.
    dead: HashSet<usize>,
}

fn build(link: &Link<'_>) -> Result<(Plan, Vec<usize>)> {
    let pointers = pointers(link, true)?;
    let mut merged = Merged::default();
    if link.config.objc_category_merging {
        merge_categories(link, &pointers, &mut merged);
    }

    let mut plan = Plan::default();
    let selrefs = if link.config.relative_method_lists {
        selrefs(link, &pointers, true)
    } else {
        HashMap::new()
    };
    let relative = |list: &List| -> Option<Rewrite> {
        let List::Methods { flags, methods } = list else {
            return None;
        };
        relative_method_list(*flags, methods, &selrefs)
    };

    // Lists merging rebuilt.
    let mut rebuilt: Vec<(&usize, &List)> = merged.lists.iter().collect();
    rebuilt.sort_by_key(|&(atom, _)| *atom);
    for (&atom, list) in rebuilt {
        let rewrite = if link.config.relative_method_lists {
            relative(list).unwrap_or_else(|| absolute_list(list))
        } else {
            absolute_list(list)
        };
        plan.rewrites.insert(atom, rewrite);
    }
    // The other method lists, when they become relative.
    if link.config.relative_method_lists {
        for atom in method_list_atoms(link, true) {
            if merged.lists.contains_key(&atom) || merged.dead.contains(&atom) {
                continue;
            }
            let Some(list) = parse_list(link, &pointers, atom, ListKind::Methods) else {
                continue;
            };
            if let Some(rewrite) = relative(&list) {
                plan.rewrites.insert(atom, rewrite);
            }
        }
    }
    // Containers with new list pointers: a copy with those fields replaced.
    for (&atom, fields) in &merged.containers {
        let Some(bytes) = atom_bytes(link, atom) else {
            continue;
        };
        let mut rewrite = Rewrite {
            bytes: bytes.to_vec(),
            align: 3,
            fields: Vec::new(),
            method_list: false,
        };
        for &(offset, target) in pointers.all(atom) {
            let renamed = offset == 0 && merged.names.contains_key(&atom);
            if renamed || fields.iter().any(|&(o, _)| o == offset) {
                continue;
            }
            rewrite.fields.push(Field {
                offset,
                kind: FieldKind::Pointer,
                target,
            });
        }
        for &(offset, list) in fields {
            let start = to_usize(offset);
            if let Some(slot) = rewrite.bytes.get_mut(start..start.saturating_add(8)) {
                slot.fill(0);
            }
            if let Some(list) = list {
                rewrite.fields.push(Field {
                    offset,
                    kind: FieldKind::Pointer,
                    target: Target {
                        place: Place::Atom {
                            atom: list,
                            offset: 0,
                        },
                        addend: 0,
                    },
                });
            }
        }
        if let Some(name) = merged.names.get(&atom) {
            let at = to_u64(rewrite.bytes.len());
            rewrite.bytes.extend_from_slice(name);
            rewrite.bytes.push(0);
            rewrite.fields.push(Field {
                offset: 0,
                kind: FieldKind::Pointer,
                target: Target {
                    place: Place::Atom {
                        atom,
                        offset: i64::try_from(at).unwrap_or(0),
                    },
                    addend: 0,
                },
            });
        }
        rewrite.fields.sort_by_key(|f| f.offset);
        clear_fields(&mut rewrite);
        plan.rewrites.insert(atom, rewrite);
    }
    // Category lists without the merged categories.
    for (&atom, slots) in &merged.catlists {
        if slots.is_empty() {
            merged.dead.insert(atom);
            continue;
        }
        let mut rewrite = Rewrite {
            bytes: vec![0; slots.len().saturating_mul(8)],
            align: 3,
            fields: Vec::new(),
            method_list: false,
        };
        for (index, &slot) in slots.iter().enumerate() {
            if let Some(target) = pointers.at(atom, slot) {
                rewrite.fields.push(Field {
                    offset: to_u64(index).saturating_mul(8),
                    kind: FieldKind::Pointer,
                    target,
                });
            }
        }
        plan.rewrites.insert(atom, rewrite);
    }
    let mut dead: Vec<usize> = merged.dead.into_iter().collect();
    dead.sort_unstable();
    for atom in &dead {
        plan.rewrites.remove(atom);
    }
    Ok((plan, dead))
}

/// Zeroes the bytes under the fields of `rewrite` (the writer fills them).
fn clear_fields(rewrite: &mut Rewrite) {
    for field in &rewrite.fields {
        let width = match field.kind {
            FieldKind::Pointer => 8,
            FieldKind::Relative => 4,
        };
        let start = to_usize(field.offset);
        if let Some(slot) = rewrite.bytes.get_mut(start..start.saturating_add(width)) {
            slot.fill(0);
        }
    }
}

/// A list in the absolute (pointer) form.
fn absolute_list(list: &List) -> Rewrite {
    let mut rewrite = Rewrite {
        align: 3,
        ..Rewrite::default()
    };
    let count = u32::try_from(list.len()).unwrap_or(u32::MAX);
    let push = |rewrite: &mut Rewrite, target: Option<Target>| {
        let offset = to_u64(rewrite.bytes.len());
        rewrite.bytes.extend_from_slice(&[0; 8]);
        if let Some(target) = target {
            rewrite.fields.push(Field {
                offset,
                kind: FieldKind::Pointer,
                target,
            });
        }
    };
    match list {
        List::Methods { methods, .. } => {
            rewrite.bytes.extend_from_slice(&24u32.to_le_bytes());
            rewrite.bytes.extend_from_slice(&count.to_le_bytes());
            for method in methods {
                push(&mut rewrite, method.name);
                push(&mut rewrite, method.types);
                push(&mut rewrite, method.imp);
            }
        }
        List::Properties(entries) => {
            rewrite.bytes.extend_from_slice(&16u32.to_le_bytes());
            rewrite.bytes.extend_from_slice(&count.to_le_bytes());
            for [name, attributes] in entries {
                push(&mut rewrite, *name);
                push(&mut rewrite, *attributes);
            }
        }
        List::Protocols(entries) => {
            rewrite
                .bytes
                .extend_from_slice(&u64::from(count).to_le_bytes());
            for &entry in entries {
                push(&mut rewrite, entry);
            }
            rewrite.bytes.extend_from_slice(&[0; 8]);
        }
    }
    rewrite
}

/// A method list in the relative form, when every method has a name with
/// a selector reference, types and an implementation.
fn relative_method_list(
    flags: u32,
    methods: &[Method],
    selrefs: &HashMap<Vec<u8>, usize>,
) -> Option<Rewrite> {
    let count = u32::try_from(methods.len()).ok()?;
    let mut rewrite = Rewrite {
        align: 2,
        method_list: true,
        ..Rewrite::default()
    };
    rewrite
        .bytes
        .extend_from_slice(&(12 | (flags & FLAGS_MASK) | RELATIVE_METHODS).to_le_bytes());
    rewrite.bytes.extend_from_slice(&count.to_le_bytes());
    for method in methods {
        let selref = *selrefs.get(method.selector.as_ref()?)?;
        let targets = [
            Target {
                place: Place::Atom {
                    atom: selref,
                    offset: 0,
                },
                addend: 0,
            },
            method.types?,
            method.imp?,
        ];
        for target in targets {
            // Offsets are 32-bit: an import cannot be reached.
            if matches!(target.place, Place::Symbol(_)) {
                return None;
            }
            rewrite.fields.push(Field {
                offset: to_u64(rewrite.bytes.len()),
                kind: FieldKind::Relative,
                target,
            });
            rewrite.bytes.extend_from_slice(&[0; 4]);
        }
    }
    Some(rewrite)
}

/// Identifies the class a category extends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum ClassKey {
    /// Defined in the image: the `class_t` atom.
    Local(usize),
    /// Defined elsewhere: the symbol.
    External(SymbolId),
}

/// Merges categories as lld does; records the results in `merged`.
fn merge_categories(link: &Link<'_>, pointers: &Pointers, merged: &mut Merged) {
    // Categories with +load stay.
    let mut nonlazy = HashSet::new();
    let mut catlists: Vec<usize> = Vec::new();
    for file in 0..link.files.len() {
        let Some(object) = link.object(file) else {
            continue;
        };
        for (section_index, section) in object.file.sections().iter().enumerate() {
            let catlist = section.sectname == b"__objc_catlist";
            if !catlist && section.sectname != b"__objc_nlcatlist" {
                continue;
            }
            let Some(range) = object.atoms.section_range(section_index) else {
                continue;
            };
            for local in range {
                if !link.is_live(file, local) {
                    continue;
                }
                let atom = link.atom_id(file, local);
                if catlist {
                    catlists.push(atom);
                } else {
                    for &(_, target) in pointers.all(atom) {
                        if let Some(body) = atom_start(target) {
                            nonlazy.insert(body);
                        }
                    }
                }
            }
        }
    }

    // Categories by class, in input order.
    let mut by_class: Vec<(ClassKey, Vec<Category>)> = Vec::new();
    let mut index_of: HashMap<ClassKey, usize> = HashMap::new();
    for &catlist in &catlists {
        let size = atom_bytes(link, catlist).map_or(0, <[u8]>::len);
        for &(offset, target) in pointers.all(catlist) {
            if to_usize(offset).saturating_add(8) > size || offset % 8 != 0 {
                continue;
            }
            let Some(body) = atom_start(target) else {
                continue;
            };
            if nonlazy.contains(&body)
                || !has_symbol_prefix(link, body, &[b"__OBJC_$_CATEGORY_"])
                || atom_bytes(link, body).is_none_or(|b| b.len() < 56)
            {
                continue;
            }
            let class = match pointers.at(body, 8).map(|t| t.place) {
                Some(Place::Atom { atom, offset: 0 }) => ClassKey::Local(atom),
                Some(Place::Symbol(id)) => ClassKey::External(id),
                _ => continue,
            };
            let category = Category {
                slot: (catlist, offset),
                body,
            };
            match index_of.get(&class) {
                Some(&index) => {
                    if let Some((_, list)) = by_class.get_mut(index) {
                        list.push(category);
                    }
                }
                None => {
                    index_of.insert(class, by_class.len());
                    by_class.push((class, vec![category]));
                }
            }
        }
    }

    let mut removed_slots: HashSet<(usize, u64)> = HashSet::new();
    for (class, categories) in &by_class {
        let done = match class {
            ClassKey::Local(class) => merge_into_class(link, pointers, *class, categories, merged),
            ClassKey::External(_) if categories.len() > 1 => {
                merge_into_category(link, pointers, categories, merged)
            }
            ClassKey::External(_) => false,
        };
        if !done {
            continue;
        }
        let keep_first = matches!(class, ClassKey::External(_));
        for (index, category) in categories.iter().enumerate() {
            if keep_first && index == 0 {
                continue;
            }
            removed_slots.insert(category.slot);
            merged.dead.insert(category.body);
        }
    }
    if removed_slots.is_empty() {
        return;
    }
    for &catlist in &catlists {
        if !removed_slots.iter().any(|&(atom, _)| atom == catlist) {
            continue;
        }
        let size = to_u64(atom_bytes(link, catlist).map_or(0, <[u8]>::len));
        let slots: Vec<u64> = (0..size / 8)
            .map(|i| i.saturating_mul(8))
            .filter(|offset| !removed_slots.contains(&(catlist, *offset)))
            .collect();
        merged.catlists.insert(catlist, slots);
    }
}

/// The list at field `field` of container atom `container`: `Ok(None)`
/// for a null field, `Err(())` for something that is not a whole list.
fn list_field(
    link: &Link<'_>,
    pointers: &Pointers,
    container: usize,
    field: u64,
    kind: ListKind,
) -> core::result::Result<Option<(usize, List)>, ()> {
    let Some(target) = pointers.at(container, field) else {
        return Ok(None);
    };
    let atom = atom_start(target).ok_or(())?;
    let list = parse_list(link, pointers, atom, kind).ok_or(())?;
    Ok(Some((atom, list)))
}

/// Accumulates one merged list: its contents and the atoms it came from.
struct Accumulator {
    kind: ListKind,
    list: Option<List>,
    /// Source atoms, the preferred storage first.
    sources: Vec<usize>,
}

impl Accumulator {
    fn new(kind: ListKind) -> Self {
        Self {
            kind,
            list: None,
            sources: Vec::new(),
        }
    }

    fn add(&mut self, source: Option<(usize, List)>) -> bool {
        let Some((atom, list)) = source else {
            return true;
        };
        if !self.sources.contains(&atom) {
            self.sources.push(atom);
        }
        match &mut self.list {
            Some(current) => current.append(&list),
            None => {
                self.list = Some(list);
                true
            }
        }
    }

    /// Stores the merged list in the first source atom (or `storage` when
    /// given and a source), drops the others; returns the list's atom.
    fn finish(self, storage: Option<usize>, merged: &mut Merged) -> Option<usize> {
        let list = self.list.filter(|l| l.len() > 0)?;
        let atom = storage
            .filter(|s| self.sources.contains(s))
            .or_else(|| self.sources.first().copied())?;
        for &source in &self.sources {
            if source != atom {
                merged.dead.insert(source);
            }
        }
        merged.lists.insert(atom, list);
        Some(atom)
    }
}

/// Merges `categories` into the class whose `class_t` is `class`.
fn merge_into_class(
    link: &Link<'_>,
    pointers: &Pointers,
    class: usize,
    categories: &[Category],
    merged: &mut Merged,
) -> bool {
    // Swift classes (and their Objective-C aliases) are left alone.
    if !has_symbol_prefix(link, class, &[b"_OBJC_CLASS_$_"])
        || has_symbol_prefix(link, class, &[b"_OBJC_CLASS_$__Tt", b"_$s"])
    {
        return false;
    }
    let ro = |class: usize| -> Option<usize> {
        let ro = atom_start(pointers.at(class, CLASS_DATA)?)?;
        (atom_bytes(link, ro)?.len() == RO_SIZE).then_some(ro)
    };
    let Some(class_ro) = ro(class) else {
        return false;
    };
    let Some(meta_ro) = pointers
        .at(class, CLASS_ISA)
        .and_then(atom_start)
        .and_then(ro)
    else {
        return false;
    };
    if atom_section_name(link, class_ro) != Some(b"__objc_const".as_slice())
        || merged.containers.contains_key(&class_ro)
    {
        return false;
    }

    let mut instance_methods = Accumulator::new(ListKind::Methods);
    let mut class_methods = Accumulator::new(ListKind::Methods);
    let mut protocols = Accumulator::new(ListKind::Protocols);
    let mut instance_props = Accumulator::new(ListKind::Properties);
    let mut class_props = Accumulator::new(ListKind::Properties);
    let mut ok = true;
    let mut add = |acc: &mut Accumulator, container: usize, field: u64| match list_field(
        link, pointers, container, field, acc.kind,
    ) {
        Ok(list) => ok &= acc.add(list),
        Err(()) => ok = false,
    };
    for category in categories {
        add(&mut instance_methods, category.body, CAT_INSTANCE_METHODS);
        add(&mut class_methods, category.body, CAT_CLASS_METHODS);
        add(&mut protocols, category.body, CAT_PROTOCOLS);
        add(&mut instance_props, category.body, CAT_INSTANCE_PROPS);
        add(&mut class_props, category.body, CAT_CLASS_PROPS);
    }
    add(&mut instance_methods, class_ro, RO_METHODS);
    add(&mut class_methods, meta_ro, RO_METHODS);
    add(&mut protocols, class_ro, RO_PROTOCOLS);
    add(&mut instance_props, class_ro, RO_PROPERTIES);
    add(&mut class_props, meta_ro, RO_PROPERTIES);
    // The metaclass must share the class's protocol list, as clang emits.
    if pointers.at(meta_ro, RO_PROTOCOLS) != pointers.at(class_ro, RO_PROTOCOLS) {
        ok = false;
    }
    if !ok {
        return false;
    }
    // Store each merged list where the class's own list was.
    let own = |container: usize, field: u64| pointers.at(container, field).and_then(atom_start);
    let instance_methods = instance_methods.finish(own(class_ro, RO_METHODS), merged);
    let class_methods = class_methods.finish(own(meta_ro, RO_METHODS), merged);
    let protocols = protocols.finish(own(class_ro, RO_PROTOCOLS), merged);
    let instance_props = instance_props.finish(own(class_ro, RO_PROPERTIES), merged);
    let class_props = class_props.finish(own(meta_ro, RO_PROPERTIES), merged);
    merged.containers.insert(
        class_ro,
        vec![
            (RO_METHODS, instance_methods),
            (RO_PROTOCOLS, protocols),
            (RO_PROPERTIES, instance_props),
        ],
    );
    merged.containers.insert(
        meta_ro,
        vec![
            (RO_METHODS, class_methods),
            (RO_PROTOCOLS, protocols),
            (RO_PROPERTIES, class_props),
        ],
    );
    true
}

/// Merges `categories` (of one class defined elsewhere) into the first.
fn merge_into_category(
    link: &Link<'_>,
    pointers: &Pointers,
    categories: &[Category],
    merged: &mut Merged,
) -> bool {
    let Some(first) = categories.first() else {
        return false;
    };
    let fields = [
        (CAT_INSTANCE_METHODS, ListKind::Methods),
        (CAT_CLASS_METHODS, ListKind::Methods),
        (CAT_PROTOCOLS, ListKind::Protocols),
        (CAT_INSTANCE_PROPS, ListKind::Properties),
        (CAT_CLASS_PROPS, ListKind::Properties),
    ];
    let mut accumulators: Vec<Accumulator> = fields
        .iter()
        .map(|&(_, kind)| Accumulator::new(kind))
        .collect();
    for category in categories {
        for (acc, &(field, kind)) in accumulators.iter_mut().zip(&fields) {
            match list_field(link, pointers, category.body, field, kind) {
                Ok(list) => {
                    if !acc.add(list) {
                        return false;
                    }
                }
                Err(()) => return false,
            }
        }
    }
    let mut container = Vec::new();
    for (acc, &(field, _)) in accumulators.into_iter().zip(&fields) {
        let own = pointers.at(first.body, field).and_then(atom_start);
        container.push((field, acc.finish(own, merged)));
    }
    let mut name = Vec::new();
    for category in categories {
        if !name.is_empty() {
            name.push(b'|');
        }
        if let Some(part) = pointers
            .at(category.body, 0)
            .and_then(|t| string_at(link, t))
        {
            name.extend_from_slice(&part);
        }
    }
    merged.names.insert(first.body, name);
    merged.containers.insert(first.body, container);
    true
}
