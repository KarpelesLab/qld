//! Objective-C relative method lists and category merging, compared with
//! `ld64.lld` (which implements both) and run on macOS.
//!
//! The metadata is compared through `llvm-objdump --objc-meta-data` on
//! outputs with rebase opcodes (`-no_fixup_chains`, so that it can follow
//! the pointers): addresses are dropped, and the entries of each section
//! are compared as sets, since lld puts a merged category's
//! `__objc_catlist` entry first. Relative method lists are also decoded
//! here, following each name offset to its selector reference and string.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use qld::macho::read::{MachOFile, Source};

use super::{
    base_args, clang_for, compile, host_can_run, ld64_lld, link_bytes, objdump, run, scratch, skip,
    strings, symbols,
};

/// Links `args` (plus `-o output`) with qld; returns the bytes.
fn link(args: &[String], output: &Path) -> Vec<u8> {
    let mut all: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
    all.push("-o".into());
    all.push(output.into());
    let (bytes, _) = link_bytes(&all).unwrap_or_else(|e| panic!("link failed: {e}"));
    std::fs::write(output, &bytes).unwrap();
    super::make_executable(output);
    bytes
}

fn link_lld(lld: &Path, args: &[String], output: &Path) {
    let result = Command::new(lld)
        .args(args)
        .arg("-o")
        .arg(output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "ld64.lld: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// The Objective-C metadata dump, without addresses, as sorted entries per
/// section.
fn metadata(file: &Path) -> Option<Vec<(String, BTreeSet<String>)>> {
    let text = objdump(&["--macho", "--objc-meta-data"], file)?;
    let mut sections: Vec<(String, BTreeSet<String>)> = Vec::new();
    let mut entry = String::new();
    let flush = |entry: &mut String, sections: &mut Vec<(String, BTreeSet<String>)>| {
        if let Some((_, set)) = sections.last_mut()
            && !entry.is_empty()
        {
            set.insert(std::mem::take(entry));
        }
        entry.clear();
    };
    for line in text.lines().skip(1) {
        let line: String = line
            .split_whitespace()
            .filter(|w| !w.starts_with("0x") && !w.starts_with("(0x"))
            .collect::<Vec<_>>()
            .join(" ");
        if line.starts_with("Contents of") {
            flush(&mut entry, &mut sections);
            sections.push((line, BTreeSet::new()));
        } else if line.len() >= 16 && line.bytes().take(16).all(|b| b.is_ascii_hexdigit()) {
            // An entry: its address and the symbol there (lld names merged
            // categories differently), then its fields.
            flush(&mut entry, &mut sections);
            entry.push_str("entry\n");
        } else {
            entry.push_str(&line);
            entry.push('\n');
        }
    }
    flush(&mut entry, &mut sections);
    Some(sections)
}

/// Reads the 8-byte pointer at `address` in an image with rebase opcodes.
fn pointer_at(data: &[u8], sections: &[(u64, u64, u64)], address: u64) -> Option<u64> {
    let (addr, _, offset) = sections
        .iter()
        .find(|&&(a, size, _)| address >= a && address < a + size)?;
    let at = (offset + (address - addr)) as usize;
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

fn string_at(data: &[u8], sections: &[(u64, u64, u64)], address: u64) -> Option<String> {
    let (addr, _, offset) = sections
        .iter()
        .find(|&&(a, size, _)| address >= a && address < a + size)?;
    let at = (offset + (address - addr)) as usize;
    let rest = data.get(at..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// The methods of the relative method lists in `__TEXT,__objc_methlist`,
/// as `selector types implementation-symbol`.
fn relative_methods(data: &[u8]) -> BTreeSet<String> {
    let file = MachOFile::parse(data, Source::new(Path::new("out"))).unwrap();
    let mut sections = Vec::new();
    let mut methlist = None;
    for command in file.load_commands() {
        if let Ok(segment) = command.unwrap().segment() {
            for section in segment.sections.iter() {
                sections.push((section.addr, section.size, u64::from(section.offset)));
                if section.sectname == b"__objc_methlist" {
                    assert_eq!(section.segname, b"__TEXT");
                    methlist = Some((section.addr, section.size, u64::from(section.offset)));
                }
            }
        }
    }
    let (addr, size, offset) = methlist.expect("__objc_methlist");
    let functions: Vec<(u64, String)> = symbols(data)
        .into_iter()
        .filter(|s| s.is_defined())
        .map(|s| (s.n_value, s.name))
        .collect();
    let read32 = |at: u64| {
        let at = at as usize;
        u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
    };
    let mut out = BTreeSet::new();
    let mut at = 0u64;
    while at < size {
        // Lists are 4-byte aligned.
        at = at.next_multiple_of(4);
        if at >= size {
            break;
        }
        let header = read32(offset + at);
        let count = read32(offset + at + 4);
        assert_eq!(header & 0x8000_ffff, 0x8000_000c, "relative, entsize 12");
        for index in 0..u64::from(count) {
            let entry = at + 8 + index * 12;
            let field = |n: u64| {
                let place = addr + entry + n * 4;
                place.wrapping_add(read32(offset + entry + n * 4) as i32 as i64 as u64)
            };
            let selref = field(0);
            let name_address = pointer_at(data, &sections, selref).expect("selector reference");
            let selector = string_at(data, &sections, name_address).expect("selector");
            let types = string_at(data, &sections, field(1)).expect("types");
            let imp = field(2);
            let function = functions
                .iter()
                .find(|(address, name)| *address == imp && name.contains('['))
                .map_or_else(|| format!("{imp:#x}"), |(_, name)| name.clone());
            out.insert(format!("{selector} {types} {function}"));
        }
        at += 8 + u64::from(count) * 12;
    }
    out
}

#[test]
fn relative_method_lists_and_category_merging() {
    // The arm64 metadata of each variant, which x86_64's must match.
    let mut arm64_metadata = std::collections::HashMap::new();
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "relative_method_lists_and_category_merging",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let object = compile("objc_categories", "objc_categories.m", arch, &[]);
        let dir = scratch("objc_categories");
        for (variant, extra) in [
            ("relative", &[][..]),
            ("merged", &["-objc_category_merging"][..]),
            ("absolute", &["-no_objc_relative_method_lists"][..]),
            (
                "absolute-merged",
                &["-no_objc_relative_method_lists", "-objc_category_merging"][..],
            ),
        ] {
            let mut args = base_args(arch);
            args.extend(strings(extra));
            args.extend(strings(&[object.to_str().unwrap(), "-lobjc", "-lSystem"]));
            let exe = dir.join(format!("{variant}-{arch}"));
            link(&args, &exe);
            if host_can_run(arch) {
                assert_eq!(run(&exe).unwrap(), "categories 305\n", "{arch} {variant}");
            }

            // Rebase opcodes, for the metadata comparisons.
            args.push("-no_fixup_chains".into());
            let legacy = dir.join(format!("{variant}-{arch}-legacy"));
            let bytes = link(&args, &legacy);
            let relative = !variant.starts_with("absolute");
            let names = super::section_names(&bytes);
            assert_eq!(
                names.iter().any(|n| n == "__objc_methlist"),
                relative,
                "{arch} {variant}: {names:?}"
            );
            if relative {
                let methods = relative_methods(&bytes);
                for wanted in [
                    "base i16@0:8 -[Base(Doubling) base]",
                    "base i16@0:8 -[Base base]",
                    "factor i16@0:8 +[Base(Doubling) factor]",
                    "second i16@0:8 +[NSObject(Second) second]",
                    "load v16@0:8 +[Base(Loading) load]",
                ] {
                    assert!(
                        methods.contains(wanted),
                        "{arch} {variant}: {wanted} in {methods:#?}"
                    );
                }
            }
            let ours = metadata(&legacy);
            if arch == "arm64" {
                arm64_metadata.insert(variant, ours.clone());
            } else if let (Some(ours), Some(Some(arm64))) = (&ours, arm64_metadata.get(variant)) {
                assert_eq!(ours, arm64, "{variant}: x86_64 vs arm64 metadata");
            }
            if variant.ends_with("merged") {
                // One category is left for NSObject and one for +load.
                let catlist = super::section_contents(&bytes, "__objc_catlist").unwrap();
                assert_eq!(catlist.len(), 16, "{arch} {variant}");
            }

            let Some(lld) = ld64_lld() else {
                continue;
            };
            let reference = dir.join(format!("{variant}-{arch}-lld"));
            link_lld(&lld, &args, &reference);
            if relative {
                let reference_bytes = std::fs::read(&reference).unwrap();
                assert_eq!(
                    relative_methods(&bytes),
                    relative_methods(&reference_bytes),
                    "{arch} {variant}: qld vs ld64.lld"
                );
            }
            // ld64.lld does not merge x86_64 categories whose lists point
            // into string sections without symbols; x86_64 is compared with
            // arm64 above instead.
            if arch == "x86_64" && variant.ends_with("merged") {
                continue;
            }
            if let (Some(ours), Some(theirs)) = (ours, metadata(&reference)) {
                assert_eq!(ours, theirs, "{arch} {variant}: qld vs ld64.lld metadata");
            }
        }
    }
}
