//! Running `readelf -W` and parsing its output into the normalized
//! properties that the differential runner compares (see `docs/testing.md`):
//!
//! - dynamic symbols with type, binding, visibility, definedness and version
//! - `DT_*` tags present, with values only where they are not addresses
//! - the multiset of dynamic relocation types against their symbols
//! - section presence, type and flags (not addresses, sizes or order)
//!
//! Each property is one line of text, so two property sets can be diffed and
//! filtered with simple substring patterns.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::process;

/// Runs `readelf -W <args> <file>` and returns its standard output.
pub fn run(readelf: &Path, args: &[String], file: &Path) -> Result<String, String> {
    let mut command = Command::new(readelf);
    command.arg("-W").args(args).arg(file).env("LC_ALL", "C");
    let output = process::run(&mut command, Duration::from_secs(60))
        .map_err(|e| format!("cannot run {}: {e}", readelf.display()))?;
    if !output.success() {
        return Err(format!(
            "readelf -W {} {} failed ({}):\n{}",
            args.join(" "),
            file.display(),
            output.describe_status(),
            output.stderr_text()
        ));
    }
    Ok(output.stdout_text())
}

/// A dynamic symbol table entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynSym {
    /// Symbol name without the version suffix.
    pub name: String,
    /// Version suffix as printed: `@VER`, `@@VER`, or empty.
    pub version: String,
    /// `FUNC`, `OBJECT`, `TLS`, `IFUNC`, `NOTYPE`, …
    pub kind: String,
    /// `GLOBAL`, `WEAK`, `LOCAL`, `UNIQUE`.
    pub bind: String,
    /// `DEFAULT`, `HIDDEN`, `PROTECTED`, `INTERNAL`.
    pub visibility: String,
    /// `UND`, `ABS`, `COM`, or `DEF` for any section index.
    pub section: String,
}

/// Parses `readelf -W --dyn-syms` output. Entry 0 and section symbols are
/// dropped.
pub fn parse_dyn_syms(text: &str) -> Vec<DynSym> {
    let mut symbols = Vec::new();
    let mut in_dynsym = false;
    for line in text.lines() {
        if line.starts_with("Symbol table '") {
            in_dynsym = line.starts_with("Symbol table '.dynsym'");
            continue;
        }
        if !in_dynsym {
            continue;
        }
        let Some((index, rest)) = line.split_once(':') else {
            continue;
        };
        let Ok(index) = index.trim().parse::<u64>() else {
            continue;
        };
        if index == 0 {
            continue;
        }
        let mut fields = rest.split_whitespace();
        let (Some(_value), Some(_size), Some(kind), Some(bind), Some(visibility)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let mut section = fields.next().unwrap_or_default().to_string();
        // Non-default st_other bits print as "[<other>: 88]" before Ndx.
        if section.starts_with('[') {
            for field in fields.by_ref() {
                if field.ends_with(']') {
                    break;
                }
            }
            section = fields.next().unwrap_or_default().to_string();
        }
        if kind == "SECTION" {
            continue;
        }
        let mut full_name = fields.collect::<Vec<_>>().join(" ");
        // Newer readelf appends the version index: "puts@GLIBC_2.2.5 (2)".
        if full_name.ends_with(')')
            && let Some(open) = full_name.rfind(" (")
        {
            full_name.truncate(open);
        }
        let (name, version) = match full_name.find('@') {
            Some(at) => (full_name[..at].to_string(), full_name[at..].to_string()),
            None => (full_name, String::new()),
        };
        let section = match section.as_str() {
            "UND" | "ABS" | "COM" => section,
            _ => "DEF".to_string(),
        };
        symbols.push(DynSym {
            name,
            version,
            kind: kind.to_string(),
            bind: bind.to_string(),
            visibility: visibility.to_string(),
            section,
        });
    }
    symbols
}

/// Parses `readelf -W -d` output into `(tag, value)` pairs, in order.
pub fn parse_dynamic(text: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if !line.starts_with("0x") {
            continue;
        }
        let (Some(open), Some(close)) = (line.find('('), line.find(')')) else {
            continue;
        };
        if close < open {
            continue;
        }
        entries.push((
            line[open + 1..close].to_string(),
            line[close + 1..].trim().to_string(),
        ));
    }
    entries
}

/// A relocation from `readelf -W -r`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reloc {
    /// Relocation section name (`.rela.dyn`, `.rela.plt`).
    pub section: String,
    /// Relocation type (`R_X86_64_GLOB_DAT`).
    pub kind: String,
    /// Symbol name, with any version suffix, or empty.
    pub symbol: String,
}

/// Parses `readelf -W -r` output.
pub fn parse_relocs(text: &str) -> Vec<Reloc> {
    let mut relocs = Vec::new();
    let mut section = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Relocation section '") {
            section = rest.split('\'').next().unwrap_or_default().to_string();
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3
            || !fields[2].starts_with("R_")
            || !fields[0].bytes().all(|b| b.is_ascii_hexdigit())
        {
            continue;
        }
        // Offset Info Type [SymValue SymName [+|- Addend]] | [Addend]
        let symbol = match fields.get(4) {
            Some(name) if *name != "+" && *name != "-" => (*name).to_string(),
            _ => String::new(),
        };
        relocs.push(Reloc {
            section: section.clone(),
            kind: fields[2].to_string(),
            symbol,
        });
    }
    relocs
}

/// A section header from `readelf -W -S`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// Section name.
    pub name: String,
    /// Section type (`PROGBITS`, `NOBITS`, …).
    pub kind: String,
    /// Flag letters (`AX`, `WA`), possibly empty.
    pub flags: String,
}

/// Parses `readelf -W -S` output. The null section is dropped.
pub fn parse_sections(text: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix('[') else {
            continue;
        };
        let Some((index, rest)) = rest.split_once(']') else {
            continue;
        };
        if index.trim().parse::<u64>().is_err() {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // Name Type Address Off Size ES [Flg] Lk Inf Al
        let (name, kind, flags) = match fields.len() {
            10 => (fields[0], fields[1], fields[6]),
            9 if fields[0] == "NULL" => continue,
            9 => (fields[0], fields[1], ""),
            _ => continue,
        };
        sections.push(Section {
            name: name.to_string(),
            kind: kind.to_string(),
            flags: flags.to_string(),
        });
    }
    sections
}

/// Dynamic tags whose values are meaningful to compare (names, flags). Every
/// other tag is compared by presence only, because its value is an address,
/// a size or a count that legitimately differs between linkers.
const SYMBOLIC_TAGS: &[&str] = &[
    "NEEDED",
    "SONAME",
    "RPATH",
    "RUNPATH",
    "AUXILIARY",
    "FILTER",
    "FLAGS",
    "FLAGS_1",
    "PLTREL",
    "BIND_NOW",
    "TEXTREL",
];

/// Computes the normalized property lines of an ELF file, sorted.
pub fn properties(readelf: &Path, file: &Path) -> Result<Vec<String>, String> {
    let arg = |a: &str| vec![a.to_string()];
    let mut lines = Vec::new();

    for symbol in parse_dyn_syms(&run(readelf, &arg("--dyn-syms"), file)?) {
        lines.push(format!(
            "dynsym: {}{} {} {} {} {}",
            symbol.name,
            symbol.version,
            symbol.kind,
            symbol.bind,
            symbol.visibility,
            symbol.section
        ));
    }

    let mut tags = BTreeMap::new();
    for (tag, value) in parse_dynamic(&run(readelf, &arg("-d"), file)?) {
        let line = if SYMBOLIC_TAGS.contains(&tag.as_str()) {
            format!("dynamic: {tag} {value}")
        } else {
            format!("dynamic: {tag}")
        };
        tags.insert(line, ());
    }
    lines.extend(tags.into_keys());

    let mut relocs: BTreeMap<String, usize> = BTreeMap::new();
    for reloc in parse_relocs(&run(readelf, &arg("-r"), file)?) {
        let symbol = if reloc.symbol.is_empty() {
            "-".to_string()
        } else {
            reloc.symbol
        };
        *relocs
            .entry(format!("reloc: {} {} {symbol}", reloc.section, reloc.kind))
            .or_default() += 1;
    }
    for (reloc, count) in relocs {
        lines.push(format!("{reloc} x{count}"));
    }

    for section in parse_sections(&run(readelf, &arg("-S"), file)?) {
        lines.push(format!(
            "section: {} {} [{}]",
            section.name, section.kind, section.flags
        ));
    }

    lines.sort();
    Ok(lines)
}

/// Compares two sorted property multisets. Returns the lines only in `left`
/// and the lines only in `right`, skipping any line containing one of the
/// `ignore` substrings.
pub fn compare(left: &[String], right: &[String], ignore: &[String]) -> (Vec<String>, Vec<String>) {
    let keep = |line: &&String| !ignore.iter().any(|pattern| line.contains(pattern.as_str()));
    let mut counts: BTreeMap<&str, isize> = BTreeMap::new();
    for line in left.iter().filter(keep) {
        *counts.entry(line).or_default() += 1;
    }
    for line in right.iter().filter(keep) {
        *counts.entry(line).or_default() -= 1;
    }
    let mut only_left = Vec::new();
    let mut only_right = Vec::new();
    for (line, count) in counts {
        for _ in 0..count.max(0) {
            only_left.push(line.to_string());
        }
        for _ in 0..(-count).max(0) {
            only_right.push(line.to_string());
        }
    }
    (only_left, only_right)
}
