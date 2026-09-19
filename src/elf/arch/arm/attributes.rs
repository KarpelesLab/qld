//! `.ARM.attributes` merging.
//!
//! Every Arm object carries a build-attributes section
//! ([`SHT_ARM_ATTRIBUTES`](super::SHT_ARM_ATTRIBUTES)) describing the
//! architecture and ABI it was built for. Concatenating them, as for an
//! ordinary section, would leave tools reading the first object's
//! attributes only, so the output gets one merged section, as GNU ld
//! writes it (lld keeps the first object's section instead):
//!
//! - architecture and extension tags (`Tag_CPU_arch`, `Tag_ARM_ISA_use`,
//!   `Tag_FP_arch`, …) take the highest value any object has, and the CPU
//!   names come from the object with the highest `Tag_CPU_arch`;
//! - ABI tags must agree: a mismatch is a warning, except
//!   `Tag_ABI_VFP_args` (the float ABI), where linking an object that
//!   passes floating-point arguments in VFP registers with one that does
//!   not is an error, as in GNU ld;
//! - tags whose value is zero ("unspecified") take the other object's.
//!
//! The merged `Tag_ABI_VFP_args` also chooses the `EF_ARM_ABI_FLOAT_HARD`
//! or `EF_ARM_ABI_FLOAT_SOFT` bit of `e_flags` ([`hard_float`]).

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::inputs::ElfInput;
use crate::elf::refs::Refs;
use crate::ids::SectionId;

use super::SHT_ARM_ATTRIBUTES;

/// `Tag_File`: attributes of the whole file.
const TAG_FILE: u8 = 1;
/// `Tag_CPU_raw_name`.
const CPU_RAW_NAME: u64 = 4;
/// `Tag_CPU_name`.
const CPU_NAME: u64 = 5;
/// `Tag_CPU_arch`.
const CPU_ARCH: u64 = 6;
/// `Tag_CPU_arch_profile`.
const CPU_ARCH_PROFILE: u64 = 7;
/// `Tag_ABI_FP_number_model`.
const ABI_FP_NUMBER_MODEL: u64 = 23;
/// `Tag_ABI_enum_size`.
const ABI_ENUM_SIZE: u64 = 26;
/// `Tag_ABI_VFP_args`: 0 base (core registers), 1 VFP registers, 2
/// toolchain-specific, 3 compatible with either.
const ABI_VFP_ARGS: u64 = 28;
/// `Tag_ABI_VFP_args` value for VFP register arguments.
const VFP_ARGS_VFP: u64 = 1;
/// `Tag_ABI_VFP_args` value for code that passes no floating-point
/// arguments.
const VFP_ARGS_COMPATIBLE: u64 = 3;
/// `Tag_compatibility`: a ULEB and a string.
const COMPATIBILITY: u64 = 32;
/// `Tag_nodefaults`, whose value carries no information.
const NODEFAULTS: u64 = 64;
/// `Tag_also_compatible_with`.
const ALSO_COMPATIBLE_WITH: u64 = 65;
/// `Tag_conformance`, which the ABI puts first.
const CONFORMANCE: u64 = 67;

/// Tags whose merged value is the highest of the objects': the
/// architecture, the extensions it may use, and the floating-point
/// properties GNU ld also takes the largest of.
const HIGHEST: &[u64] = &[
    CPU_ARCH, 8,  // Tag_ARM_ISA_use
    9,  // Tag_THUMB_ISA_use
    10, // Tag_FP_arch
    11, // Tag_WMMX_arch
    12, // Tag_Advanced_SIMD_arch
    19, // Tag_ABI_FP_rounding
    20, // Tag_ABI_FP_denormal
    21, // Tag_ABI_FP_exceptions
    22, // Tag_ABI_FP_user_exceptions
    23, // Tag_ABI_FP_number_model
    24, // Tag_ABI_align_needed
    25, // Tag_ABI_align_preserved
    27, // Tag_ABI_HardFP_use
    34, // Tag_CPU_unaligned_access
    36, // Tag_FP_HP_extension
    42, // Tag_MPextension_use
    44, // Tag_DIV_use
    46, // Tag_DSP_extension
    48, // Tag_MVE_arch
    50, // Tag_PAC_extension
    52, // Tag_BTI_extension
    66, // Tag_T2EE_use
    68, // Tag_Virtualization_use
    70, // Tag_MPextension_use (legacy)
    74, // Tag_BTI_use
    76, // Tag_PACRET_use
];

/// A value of one attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    /// A ULEB128.
    Int(u64),
    /// A NUL-terminated string.
    Text(Vec<u8>),
    /// `Tag_compatibility`: a ULEB128 and a string.
    Pair(u64, Vec<u8>),
}

/// The `aeabi` file attributes of one section, in file order.
type Attributes = Vec<(u64, Value)>;

/// The merged `.ARM.attributes` of a link.
#[derive(Clone, Debug, Default)]
pub struct Output {
    /// The input section whose place in the output holds the merged
    /// section; the others are written empty.
    pub first: SectionId,
    /// The merged contents.
    pub bytes: Vec<u8>,
    /// Warnings found while merging.
    pub problems: Vec<String>,
    /// Errors found while merging (a float ABI mismatch).
    pub errors: Vec<String>,
    /// Whether the merged `Tag_ABI_VFP_args` says floating-point
    /// arguments go in VFP registers.
    pub hard_float: bool,
}

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

fn text(data: &[u8], at: &mut usize) -> Option<Vec<u8>> {
    let rest = data.get(*at..)?;
    let len = rest.iter().position(|&b| b == 0)?;
    *at = at.checked_add(len)?.checked_add(1)?;
    Some(rest.get(..len)?.to_vec())
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

/// Whether tag `tag` holds a string rather than a ULEB128: the named
/// string tags, and by the ABI's rule odd tags above 32.
fn is_text(tag: u64) -> bool {
    match tag {
        CPU_RAW_NAME | CPU_NAME | ALSO_COMPATIBLE_WITH | CONFORMANCE => true,
        COMPATIBILITY | NODEFAULTS => false,
        _ => tag > 32 && tag % 2 == 1,
    }
}

/// Parses the `aeabi` vendor's file-level attributes of a section.
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
        if body.get(..nul) != Some(b"aeabi".as_slice()) {
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
                    let value = if key == COMPATIBILITY {
                        let flag = uleb(attrs, &mut pos).ok_or("truncated attribute")?;
                        let vendor = text(attrs, &mut pos).ok_or("unterminated attribute")?;
                        Value::Pair(flag, vendor)
                    } else if is_text(key) {
                        Value::Text(text(attrs, &mut pos).ok_or("unterminated attribute")?)
                    } else {
                        Value::Int(uleb(attrs, &mut pos).ok_or("truncated attribute")?)
                    };
                    out.push((key, value));
                }
            }
            inner = stop;
        }
    }
    Ok(out)
}

/// The integer value of `tag`, or 0 when the object does not set it.
fn int(attrs: &Attributes, tag: u64) -> u64 {
    attrs
        .iter()
        .find(|(key, _)| *key == tag)
        .and_then(|(_, value)| match value {
            Value::Int(value) => Some(*value),
            _ => None,
        })
        .unwrap_or(0)
}

/// Whether the merged objects pass floating-point arguments in VFP
/// registers, which selects `EF_ARM_ABI_FLOAT_HARD`.
#[must_use]
pub fn hard_float<F: crate::elf::read::ElfFormat>(files: &[ElfInput<'_, F>]) -> bool {
    for file in files {
        let Some(object) = &file.object else {
            continue;
        };
        for section in &object.sections {
            if section.header.sh_type != SHT_ARM_ATTRIBUTES {
                continue;
            }
            let Ok(data) = object.elf.section_data(&section.header) else {
                continue;
            };
            if let Ok(attrs) = parse(data)
                && int(&attrs, ABI_VFP_ARGS) == VFP_ARGS_VFP
            {
                return true;
            }
        }
    }
    false
}

/// Merges `attrs` into `out`, reporting problems about `name`.
fn merge_into(out: &mut Attributes, attrs: &Attributes, name: &str, result: &mut Output) {
    // The float ABI: GNU ld's rule, which ignores objects that use no
    // floating point at all.
    let theirs = int(attrs, ABI_VFP_ARGS);
    let ours = int(out, ABI_VFP_ARGS);
    if theirs != ours {
        let their_model = int(attrs, ABI_FP_NUMBER_MODEL);
        let our_model = int(out, ABI_FP_NUMBER_MODEL);
        if our_model == 0 || (their_model != 0 && ours == VFP_ARGS_COMPATIBLE) {
            set(out, ABI_VFP_ARGS, Value::Int(theirs));
        } else if their_model != 0 && theirs != VFP_ARGS_COMPATIBLE {
            let (vfp, soft) = if theirs == VFP_ARGS_VFP {
                (name, "the output")
            } else {
                ("the output", name)
            };
            result.errors.push(format!(
                "{vfp} uses VFP register arguments, {soft} does not"
            ));
        }
    }
    let their_arch = int(attrs, CPU_ARCH);
    let our_arch = int(out, CPU_ARCH);
    for (tag, value) in attrs {
        let tag = *tag;
        if tag == ABI_VFP_ARGS || tag == NODEFAULTS {
            continue;
        }
        let current = out.iter().find(|(key, _)| *key == tag).map(|(_, v)| v);
        let Some(current) = current else {
            set(out, tag, value.clone());
            continue;
        };
        if current == value {
            continue;
        }
        match (current, value) {
            // The CPU names describe the architecture the merged object
            // claims, so they come from the object that has the highest.
            _ if matches!(tag, CPU_NAME | CPU_RAW_NAME) => {
                if their_arch > our_arch {
                    set(out, tag, value.clone());
                }
            }
            (Value::Int(current), Value::Int(their)) => {
                let current = *current;
                let their = *their;
                let merged = if HIGHEST.contains(&tag) || tag == CPU_ARCH_PROFILE {
                    current.max(their)
                } else if current == 0 {
                    their
                } else if their == 0 {
                    current
                } else {
                    if tag == ABI_ENUM_SIZE && (current == 3 || their == 3) {
                        // "Any size" is compatible with both.
                        set(out, tag, Value::Int(current.min(their)));
                        continue;
                    }
                    result.problems.push(format!(
                        "{name}: attribute {tag} is {their}, the output has {current}"
                    ));
                    current
                };
                set(out, tag, Value::Int(merged));
            }
            // Strings and `Tag_compatibility`: the first object's.
            _ => {}
        }
    }
    if their_arch > our_arch {
        set(out, CPU_ARCH, Value::Int(their_arch));
    }
}

fn set(out: &mut Attributes, tag: u64, value: Value) {
    match out.iter_mut().find(|(key, _)| *key == tag) {
        Some(slot) => slot.1 = value,
        None => out.push((tag, value)),
    }
}

/// Encodes merged attributes as a `.ARM.attributes` section.
fn encode(attrs: &Attributes) -> Vec<u8> {
    let mut body = Vec::new();
    // The ABI asks for `Tag_conformance` first; everything else follows in
    // tag order, as GNU ld writes it.
    let mut tags: Vec<&(u64, Value)> = attrs.iter().collect();
    tags.sort_by_key(|(tag, _)| (*tag != CONFORMANCE, *tag));
    for (tag, value) in tags {
        push_uleb(&mut body, *tag);
        match value {
            Value::Int(value) => push_uleb(&mut body, *value),
            Value::Text(text) => {
                body.extend_from_slice(text);
                body.push(0);
            }
            Value::Pair(flag, text) => {
                push_uleb(&mut body, *flag);
                body.extend_from_slice(text);
                body.push(0);
            }
        }
    }
    let mut out = vec![b'A'];
    let subsection = 4usize
        .saturating_add(b"aeabi\0".len())
        .saturating_add(5)
        .saturating_add(body.len());
    out.extend_from_slice(&(subsection as u32).to_le_bytes());
    out.extend_from_slice(b"aeabi\0");
    out.push(TAG_FILE);
    out.extend_from_slice(&(body.len().saturating_add(5) as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Merges the `.ARM.attributes` sections of the live inputs, in input
/// order. `None` when there is none.
#[must_use]
pub fn collect<F: crate::elf::read::ElfFormat>(refs: &Refs<'_, '_, F>) -> Option<Output> {
    let mut result = Output::default();
    let mut merged: Option<Attributes> = None;
    for (file_index, file) in refs.files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for (index, section) in object.sections.iter().enumerate() {
            if section.header.sh_type != SHT_ARM_ATTRIBUTES {
                continue;
            }
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let Some(id) = refs.sections.id(file_index, index) else {
                continue;
            };
            if !refs.sections.is_live(id) {
                continue;
            }
            let data = object.elf.section_data(&section.header).unwrap_or_default();
            let name = file.display();
            let attrs = match parse(data) {
                Ok(attrs) => attrs,
                Err(problem) => {
                    result
                        .problems
                        .push(format!("{name}: {problem} in .ARM.attributes"));
                    continue;
                }
            };
            match &mut merged {
                None => {
                    result.first = id;
                    merged = Some(attrs);
                }
                Some(merged) => merge_into(merged, &attrs, &name, &mut result),
            }
        }
    }
    let merged = merged?;
    result.hard_float = int(&merged, ABI_VFP_ARGS) == VFP_ARGS_VFP;
    result.bytes = encode(&merged);
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A section holding `attrs` (tag, value) pairs as integers.
    fn section(attrs: &[(u64, u64)]) -> Vec<u8> {
        let list: Attributes = attrs.iter().map(|&(t, v)| (t, Value::Int(v))).collect();
        encode(&list)
    }

    #[test]
    fn parses_what_it_encodes() {
        let data = section(&[(CPU_ARCH, 10), (ABI_VFP_ARGS, 1), (34, 1)]);
        assert_eq!(data.first(), Some(&b'A'));
        let attrs = parse(&data).unwrap();
        assert_eq!(int(&attrs, CPU_ARCH), 10);
        assert_eq!(int(&attrs, ABI_VFP_ARGS), 1);
        assert_eq!(
            parse(b"X").unwrap_err(),
            "unknown attributes format version"
        );
        assert!(parse(&[]).unwrap().is_empty());
        assert!(parse(b"A\x04\0\0\0").is_err());
    }

    #[test]
    fn keeps_strings_and_the_conformance_tag_first() {
        let attrs: Attributes = vec![
            (CPU_ARCH, Value::Int(10)),
            (CONFORMANCE, Value::Text(b"2.09".to_vec())),
            (CPU_NAME, Value::Text(b"Cortex-A9".to_vec())),
        ];
        let data = encode(&attrs);
        // 'A', the subsection length, "aeabi\0", `Tag_File` and its size.
        assert_eq!(data.get(16), Some(&(CONFORMANCE as u8)));
        let back = parse(&data).unwrap();
        assert_eq!(back.first().map(|(tag, _)| *tag), Some(CONFORMANCE));
        assert_eq!(back.len(), 3);
    }

    fn merge(a: &[(u64, u64)], b: &[(u64, u64)]) -> (Attributes, Output) {
        let mut out = parse(&section(a)).unwrap();
        let mut result = Output::default();
        let second = parse(&section(b)).unwrap();
        merge_into(&mut out, &second, "b.o", &mut result);
        (out, result)
    }

    #[test]
    fn takes_the_highest_architecture() {
        let (merged, result) = merge(
            &[(CPU_ARCH, 8), (8, 1), (9, 2)],
            &[(CPU_ARCH, 10), (8, 1), (9, 2), (10, 3)],
        );
        assert_eq!(int(&merged, CPU_ARCH), 10);
        assert_eq!(int(&merged, 10), 3);
        assert!(result.problems.is_empty() && result.errors.is_empty());
    }

    #[test]
    fn reports_a_float_abi_mismatch() {
        let hard = [(ABI_FP_NUMBER_MODEL, 3), (ABI_VFP_ARGS, 1)];
        let soft = [(ABI_FP_NUMBER_MODEL, 3), (ABI_VFP_ARGS, 0)];
        let (_, result) = merge(&hard, &soft);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("VFP register arguments"));
        // An object that uses no floating point merges with either.
        let (merged, result) = merge(&hard, &[(ABI_VFP_ARGS, 0)]);
        assert!(result.errors.is_empty());
        assert_eq!(int(&merged, ABI_VFP_ARGS), 1);
        let (merged, result) = merge(&[(ABI_VFP_ARGS, 0)], &hard);
        assert!(result.errors.is_empty());
        assert_eq!(int(&merged, ABI_VFP_ARGS), 1);
    }

    #[test]
    fn warns_about_abi_mismatches() {
        let (merged, result) = merge(&[(18, 4)], &[(18, 2)]);
        assert_eq!(int(&merged, 18), 4);
        assert_eq!(result.problems.len(), 1);
        // "Any size" enums merge with both.
        let (merged, result) = merge(&[(ABI_ENUM_SIZE, 3)], &[(ABI_ENUM_SIZE, 1)]);
        assert_eq!(int(&merged, ABI_ENUM_SIZE), 1);
        assert!(result.problems.is_empty());
    }
}
