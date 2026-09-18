//! `.riscv.attributes` merging.
//!
//! Every object carries a build-attributes section (`SHT_RISCV_ATTRIBUTES`)
//! naming the ISA it was compiled for. Concatenating them, as for an
//! ordinary section, would give tools such as `objdump` the first object's
//! ISA only, so the output gets one merged section, as lld and GNU ld write
//! it:
//!
//! - `Tag_RISCV_arch`: the union of the extensions, each at the highest
//!   version any object has, in canonical order;
//! - `Tag_RISCV_stack_align`: must agree (a mismatch is reported);
//! - `Tag_RISCV_unaligned_access`: set if any object sets it;
//! - `Tag_RISCV_atomic_abi`: the psABI's compatibility rules;
//! - other tags: kept when every object that has them agrees.
//!
//! Attributes are written in ascending tag order, as GNU ld does.

#![deny(clippy::arithmetic_side_effects)]

use std::collections::BTreeMap;

use crate::elf::read::consts::SHT_RISCV_ATTRIBUTES;
use crate::elf::refs::Refs;
use crate::ids::SectionId;

/// `Tag_File`: attributes of the whole file.
const TAG_FILE: u8 = 1;
/// `Tag_RISCV_stack_align`.
const STACK_ALIGN: u64 = 4;
/// `Tag_RISCV_arch`.
const ARCH: u64 = 5;
/// `Tag_RISCV_unaligned_access`.
const UNALIGNED_ACCESS: u64 = 6;
/// `Tag_RISCV_atomic_abi`.
const ATOMIC_ABI: u64 = 14;

/// A value of one attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Int(u64),
    Text(Vec<u8>),
}

/// The attributes of one section, in file order.
type Attributes = Vec<(u64, Value)>;

fn uleb(data: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *data.get(*at)?;
        *at = at.checked_add(1)?;
        if shift < 64 {
            value |= u64::from(byte & 0x7f).checked_shl(shift).unwrap_or(0);
        }
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift = shift.saturating_add(7);
    }
}

fn push_uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Parses the `riscv` vendor's file-level attributes of a section.
///
/// # Errors
///
/// A description of the first malformation found.
fn parse(data: &[u8]) -> Result<Attributes, &'static str> {
    let mut out = Vec::new();
    match data.first() {
        None => return Ok(out),
        Some(b'A') => {}
        Some(_) => return Err("unknown attributes format version"),
    }
    let mut at = 1usize;
    while at < data.len() {
        let length = data
            .get(at..)
            .and_then(|r| r.first_chunk::<4>())
            .map(|w| u32::from_le_bytes(*w) as usize)
            .ok_or("truncated attributes subsection")?;
        let end = at
            .checked_add(length)
            .filter(|&end| end <= data.len() && length >= 4)
            .ok_or("attributes subsection overruns the section")?;
        let body = data.get(at.saturating_add(4)..end).unwrap_or_default();
        at = end;
        let Some(nul) = body.iter().position(|&b| b == 0) else {
            return Err("unterminated attributes vendor name");
        };
        if body.get(..nul) != Some(b"riscv".as_slice()) {
            continue;
        }
        let mut inner = nul.saturating_add(1);
        while inner < body.len() {
            let tag = *body.get(inner).ok_or("truncated attributes")?;
            let size = body
                .get(inner.saturating_add(1)..)
                .and_then(|r| r.first_chunk::<4>())
                .map(|w| u32::from_le_bytes(*w) as usize)
                .ok_or("truncated attributes")?;
            let stop = inner
                .checked_add(size)
                .filter(|&stop| stop <= body.len() && size >= 5)
                .ok_or("attributes overrun their subsection")?;
            if tag == TAG_FILE {
                let attrs = body.get(inner.saturating_add(5)..stop).unwrap_or_default();
                let mut pos = 0usize;
                while pos < attrs.len() {
                    let key = uleb(attrs, &mut pos).ok_or("truncated attribute")?;
                    // Even tags hold numbers, odd ones strings (the psABI
                    // rule for tags qld does not know).
                    if key % 2 == 0 {
                        let value = uleb(attrs, &mut pos).ok_or("truncated attribute")?;
                        out.push((key, Value::Int(value)));
                    } else {
                        let rest = attrs.get(pos..).unwrap_or_default();
                        let len = rest
                            .iter()
                            .position(|&b| b == 0)
                            .ok_or("unterminated attribute string")?;
                        out.push((
                            key,
                            Value::Text(rest.get(..len).unwrap_or_default().to_vec()),
                        ));
                        pos = pos.saturating_add(len).saturating_add(1);
                    }
                }
            }
            inner = stop;
        }
    }
    Ok(out)
}

/// One extension of an ISA string with its version.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Extension {
    name: String,
    major: u32,
    minor: u32,
}

/// The canonical position of a single-letter extension.
fn letter_rank(letter: u8) -> u32 {
    match letter {
        b'i' => 0,
        b'e' => 1,
        _ => match b"mafdqlcbkjtpvnh".iter().position(|&c| c == letter) {
            Some(position) => u32::try_from(position).unwrap_or(0).saturating_add(2),
            None => 17u32.saturating_add(u32::from(letter.saturating_sub(b'a'))),
        },
    }
}

/// The canonical order of extensions (LLVM's `RISCVISAUtils` order): single
/// letters, then `z*` by the category of their second letter, then `s*`,
/// then `x*`, each group alphabetically.
fn rank(name: &str) -> (u32, &str) {
    let bytes = name.as_bytes();
    let rank = match bytes {
        [b's', ..] => 1 << 7,
        [b'z', second, ..] => (1 << 6) | letter_rank(*second),
        [b'x', ..] => 1 << 8,
        [letter] => letter_rank(*letter),
        _ => 1 << 9,
    };
    (rank, name)
}

/// Splits a normalized ISA string (`rv64i2p1_m2p0_zicsr2p0`) into its base
/// width and extensions.
fn parse_arch(text: &str) -> Option<(u32, Vec<Extension>)> {
    let rest = text.strip_prefix("rv")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let xlen: u32 = rest.get(..digits)?.parse().ok()?;
    let rest = rest.get(digits..)?;
    let mut out = Vec::new();
    for part in rest.split('_').filter(|p| !p.is_empty()) {
        // The name, then `<major>p<minor>`; names may contain digits
        // (`zve32x`), so the version is found from the end.
        let digits_before = |end: usize| {
            part.get(..end).map_or(0, |s| {
                s.bytes().rev().take_while(u8::is_ascii_digit).count()
            })
        };
        let minor_len = digits_before(part.len());
        let p_at = part.len().checked_sub(minor_len)?.checked_sub(1);
        let (name, major, minor) = match p_at {
            Some(p_at) if minor_len > 0 && part.as_bytes().get(p_at) == Some(&b'p') => {
                let major_len = digits_before(p_at);
                let name_end = p_at.checked_sub(major_len)?;
                if major_len == 0 || name_end == 0 {
                    return None;
                }
                (
                    part.get(..name_end)?,
                    part.get(name_end..p_at)?.parse().ok()?,
                    part.get(p_at.checked_add(1)?..)?.parse().ok()?,
                )
            }
            _ => (part, 0, 0),
        };
        out.push(Extension {
            name: name.to_string(),
            major,
            minor,
        });
    }
    Some((xlen, out))
}

/// Merges ISA strings: every extension, at the highest version.
fn merge_arch(strings: &[&[u8]]) -> Result<Option<Vec<u8>>, String> {
    let mut xlen = None;
    let mut merged: BTreeMap<(u32, String), (u32, u32)> = BTreeMap::new();
    for text in strings {
        let text = String::from_utf8_lossy(text);
        let (width, extensions) =
            parse_arch(&text).ok_or_else(|| format!("{text}: invalid ISA string"))?;
        xlen.get_or_insert(width);
        for ext in extensions {
            let key = {
                let (rank, name) = rank(&ext.name);
                (rank, name.to_string())
            };
            let slot = merged.entry(key).or_insert((ext.major, ext.minor));
            if (ext.major, ext.minor) > *slot {
                *slot = (ext.major, ext.minor);
            }
        }
    }
    let Some(xlen) = xlen else {
        return Ok(None);
    };
    let mut out = format!("rv{xlen}");
    for (index, ((_, name), (major, minor))) in merged.iter().enumerate() {
        if index > 0 {
            out.push('_');
        }
        out.push_str(&format!("{name}{major}p{minor}"));
    }
    Ok(Some(out.into_bytes()))
}

/// The merged attributes, and problems found while merging.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Merged {
    /// The section contents.
    pub bytes: Vec<u8>,
    /// Conflicts between inputs, to report as warnings.
    pub problems: Vec<String>,
}

/// Atomic ABI tags.
const ATOMIC_UNKNOWN: u64 = 0;
const ATOMIC_A6C: u64 = 1;
const ATOMIC_A6S: u64 = 2;
const ATOMIC_A7: u64 = 3;

/// Merges two atomic ABIs, or `None` if they are incompatible.
fn merge_atomic(old: u64, new: u64) -> Option<u64> {
    if old == new || new == ATOMIC_UNKNOWN {
        return Some(old);
    }
    match (old, new) {
        (ATOMIC_UNKNOWN, _) => Some(new),
        (ATOMIC_A6C, ATOMIC_A6S) | (ATOMIC_A6S, ATOMIC_A6C) => Some(ATOMIC_A6C),
        (ATOMIC_A6S, ATOMIC_A7) | (ATOMIC_A7, ATOMIC_A6S) => Some(ATOMIC_A7),
        _ => None,
    }
}

/// Merges the contents of the input `.riscv.attributes` sections, named by
/// `(section, contents)` for diagnostics.
#[must_use]
pub fn merge(sections: &[(String, &[u8])]) -> Merged {
    let mut problems = Vec::new();
    let mut ints: BTreeMap<u64, (u64, usize)> = BTreeMap::new();
    let mut texts: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut arches: Vec<&[u8]> = Vec::new();
    let mut parsed = Vec::with_capacity(sections.len());
    for (name, data) in sections {
        match parse(data) {
            Ok(attrs) => parsed.push(attrs),
            Err(what) => {
                problems.push(format!("{name}: {what}"));
                parsed.push(Vec::new());
            }
        }
    }
    for (index, attrs) in parsed.iter().enumerate() {
        for (tag, value) in attrs {
            match (*tag, value) {
                (ARCH, Value::Text(text)) => arches.push(text),
                (STACK_ALIGN, Value::Int(v)) => match ints.get(&STACK_ALIGN) {
                    Some(&(first, from)) if first != *v => problems.push(format!(
                        "{} has stack_align={v} but {} has stack_align={first}",
                        sections.get(index).map_or("", |s| s.0.as_str()),
                        sections.get(from).map_or("", |s| s.0.as_str()),
                    )),
                    Some(_) => {}
                    None => {
                        ints.insert(STACK_ALIGN, (*v, index));
                    }
                },
                (UNALIGNED_ACCESS, Value::Int(v)) => {
                    let slot = ints.entry(UNALIGNED_ACCESS).or_insert((0, index));
                    slot.0 |= *v;
                }
                (ATOMIC_ABI, Value::Int(v)) => match ints.get_mut(&ATOMIC_ABI) {
                    Some(slot) => match merge_atomic(slot.0, *v) {
                        Some(merged) => slot.0 = merged,
                        None => problems.push(format!(
                            "atomic abi mismatch for .riscv.attributes: {} has atomic_abi={} but {} has atomic_abi={v}",
                            sections.get(slot.1).map_or("", |s| s.0.as_str()),
                            slot.0,
                            sections.get(index).map_or("", |s| s.0.as_str()),
                        )),
                    },
                    None => {
                        ints.insert(ATOMIC_ABI, (*v, index));
                    }
                },
                // Anything else is kept while every section agrees.
                (tag, Value::Int(v)) => {
                    let slot = ints.entry(tag).or_insert((*v, index));
                    if slot.0 != *v {
                        slot.0 = 0;
                    }
                }
                (tag, Value::Text(text)) => {
                    let slot = texts.entry(tag).or_insert_with(|| text.clone());
                    if slot != text {
                        slot.clear();
                    }
                }
            }
        }
    }
    match merge_arch(&arches) {
        Ok(Some(arch)) => {
            texts.insert(ARCH, arch);
        }
        Ok(None) => {}
        Err(what) => problems.push(what),
    }

    let mut body: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for (&tag, &(value, _)) in &ints {
        if value != 0 {
            let mut bytes = Vec::new();
            push_uleb(&mut bytes, value);
            body.insert(tag, bytes);
        }
    }
    for (&tag, text) in &texts {
        if !text.is_empty() {
            let mut bytes = text.clone();
            bytes.push(0);
            body.insert(tag, bytes);
        }
    }
    if body.is_empty() && sections.is_empty() {
        return Merged {
            bytes: Vec::new(),
            problems,
        };
    }
    let mut attrs = Vec::new();
    for (tag, value) in body {
        push_uleb(&mut attrs, tag);
        attrs.extend_from_slice(&value);
    }
    // 'A', subsection length, "riscv\0", Tag_File, its length, attributes.
    let file_len = attrs.len().saturating_add(5);
    let sub_len = file_len.saturating_add(4 + 6);
    let mut bytes = Vec::with_capacity(sub_len.saturating_add(1));
    bytes.push(b'A');
    bytes.extend_from_slice(&u32::try_from(sub_len).unwrap_or(u32::MAX).to_le_bytes());
    bytes.extend_from_slice(b"riscv\0");
    bytes.push(TAG_FILE);
    bytes.extend_from_slice(&u32::try_from(file_len).unwrap_or(u32::MAX).to_le_bytes());
    bytes.extend_from_slice(&attrs);
    Merged { bytes, problems }
}

/// The merged `.riscv.attributes` of a link, written in place of the first
/// input section; the others become empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The input section whose place the merged section takes.
    pub first: SectionId,
    /// The merged contents.
    pub bytes: Vec<u8>,
    /// Conflicts between inputs, reported as warnings when written.
    pub problems: Vec<String>,
}

/// Merges the live `SHT_RISCV_ATTRIBUTES` sections of every object, in
/// input order; `None` when there are none.
#[must_use]
pub fn collect(refs: &Refs<'_, '_>) -> Option<Output> {
    let mut first = None;
    let mut sections: Vec<(String, &[u8])> = Vec::new();
    for (file_index, file) in refs.files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for (index, section) in object.sections.iter().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            if section.header.sh_type != SHT_RISCV_ATTRIBUTES
                || !refs.sections.is_live_in(file_index, index)
            {
                continue;
            }
            let Some(id) = refs.sections.id(file_index, index) else {
                continue;
            };
            first.get_or_insert(id);
            let data = object.section_data(section).unwrap_or_default();
            sections.push((file.display(), data));
        }
    }
    let first = first?;
    let merged = merge(&sections);
    Some(Output {
        first,
        bytes: merged.bytes,
        problems: merged.problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(attrs: &[(u64, Value)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (tag, value) in attrs {
            push_uleb(&mut body, *tag);
            match value {
                Value::Int(v) => push_uleb(&mut body, *v),
                Value::Text(t) => {
                    body.extend_from_slice(t);
                    body.push(0);
                }
            }
        }
        let mut out = vec![b'A'];
        out.extend_from_slice(
            &u32::try_from(body.len().saturating_add(15))
                .unwrap()
                .to_le_bytes(),
        );
        out.extend_from_slice(b"riscv\0");
        out.push(TAG_FILE);
        out.extend_from_slice(
            &u32::try_from(body.len().saturating_add(5))
                .unwrap()
                .to_le_bytes(),
        );
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn arch_strings_are_unioned_in_canonical_order() {
        let a = section(&[
            (STACK_ALIGN, Value::Int(16)),
            (
                ARCH,
                Value::Text(b"rv64i2p1_m2p0_a2p1_c2p0_zicsr2p0".to_vec()),
            ),
        ]);
        let b = section(&[
            (STACK_ALIGN, Value::Int(16)),
            (
                ARCH,
                Value::Text(b"rv64i2p0_m2p0_f2p2_d2p2_zifencei2p0_zmmul1p0_zve32x1p0".to_vec()),
            ),
            (UNALIGNED_ACCESS, Value::Int(1)),
        ]);
        let merged = merge(&[("a.o".into(), &a), ("b.o".into(), &b)]);
        assert!(merged.problems.is_empty(), "{:?}", merged.problems);
        let parsed = parse(&merged.bytes).unwrap();
        assert_eq!(
            parsed,
            [
                (STACK_ALIGN, Value::Int(16)),
                (
                    ARCH,
                    Value::Text(
                        b"rv64i2p1_m2p0_a2p1_f2p2_d2p2_c2p0_zicsr2p0_zifencei2p0_zmmul1p0_zve32x1p0".to_vec()
                    )
                ),
                (UNALIGNED_ACCESS, Value::Int(1)),
            ]
        );
    }

    #[test]
    fn conflicts_are_reported() {
        let a = section(&[(STACK_ALIGN, Value::Int(16)), (ATOMIC_ABI, Value::Int(1))]);
        let b = section(&[(STACK_ALIGN, Value::Int(8)), (ATOMIC_ABI, Value::Int(3))]);
        let merged = merge(&[("a.o".into(), &a), ("b.o".into(), &b)]);
        assert_eq!(merged.problems.len(), 2, "{:?}", merged.problems);
        assert!(merged.problems[0].contains("stack_align"));
    }

    #[test]
    fn malformed_sections_do_not_panic() {
        let good = section(&[(ARCH, Value::Text(b"rv64i2p1".to_vec()))]);
        for cut in 0..good.len() {
            let _ = merge(&[("x.o".into(), &good[..cut])]);
        }
        let merged = merge(&[("x.o".into(), b"B")]);
        assert_eq!(merged.problems.len(), 1);
    }
}
