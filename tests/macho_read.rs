//! Integration tests for the Mach-O reader (`qld::macho::read`, workstream
//! W15).
//!
//! Prebuilt fixtures under `tests/data/macho_read/` (see `regen.sh` there)
//! keep the core tests running on every host. When LLVM tools are installed,
//! the tests also compare what qld parses with `obj2yaml`, `llvm-objdump`,
//! `llvm-dwarfdump` and `llvm-readtapi`, and compile fresh objects with
//! `clang --target=*-apple-macos*` (no SDK needed). Missing tools make those
//! checks skip with a message.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use qld::input::archive::Archive;
use qld::macho::read::consts::{
    self, ARM64_RELOC_ADDEND, ARM64_RELOC_PAGEOFF12, ARM64_RELOC_SUBTRACTOR,
    ARM64_RELOC_TLVP_LOAD_PAGE21, CPU_TYPE_ARM64, CPU_TYPE_X86, CPU_TYPE_X86_64,
    DICE_KIND_JUMP_TABLE32, GENERIC_RELOC_PAIR, MH_DYLIB, PLATFORM_MACOS, S_ATTR_NO_DEAD_STRIP,
    S_THREAD_LOCAL_REGULAR, S_THREAD_LOCAL_VARIABLES, S_THREAD_LOCAL_ZEROFILL, X86_64_RELOC_TLV,
};
use qld::macho::read::tbd::{StubLibrary, StubSymbolKind, StubTarget, TextStub};
use qld::macho::read::{
    Arch, AtomKind, Atomization, Dylib, DylibLoadKind, EhFrame, EhFrameKind, ExportTarget, FatFile,
    LinkerOptionHint, ObjectFile, PackedVersion, RelocationTarget, Source, compact_unwind_entries,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/macho_read")
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(data_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn scratch_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("macho_read");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn tool_works(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Runs `tool args path`, returning stdout, or `None` (with a message) when
/// the tool is missing or fails.
fn run_tool(tool: &str, args: &[&str], path: &Path) -> Option<String> {
    if !tool_works(tool) {
        println!("SKIPPED: {tool} is not installed");
        return None;
    }
    let output = Command::new(tool).args(args).arg(path).output().ok()?;
    if !output.status.success() {
        println!(
            "SKIPPED: `{tool} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Compiles `source` with `compiler args -c source -o out`.
fn compile(compiler: &str, args: &[&str], source: &str, out: &str) -> Option<PathBuf> {
    if !tool_works(compiler) {
        println!("SKIPPED: {compiler} is not installed");
        return None;
    }
    let out = scratch_dir().join(out);
    let output = Command::new(compiler)
        .args(args)
        .arg("-c")
        .arg(data_dir().join(source))
        .arg("-o")
        .arg(&out)
        .output()
        .ok()?;
    if !output.status.success() {
        println!(
            "SKIPPED: `{compiler} {} {source}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    Some(out)
}

fn src(path: &Path) -> Source<'_> {
    Source::new(path)
}

fn parse_num(text: &str) -> u64 {
    let text = text.trim().trim_matches('\'');
    match text {
        "true" => 1,
        "false" | "" => 0,
        _ => text
            .strip_prefix("0x")
            .map(|hex| u64::from_str_radix(hex, 16))
            .unwrap_or_else(|| text.parse::<i64>().map(|v| v as u64))
            .unwrap_or_else(|e| panic!("bad number {text:?}: {e}")),
    }
}

fn find_symbol<'a>(object: &ObjectFile<'a>, name: &str) -> qld::macho::read::Symbol<'a> {
    object
        .symbols()
        .iter()
        .map(Result::unwrap)
        .find(|s| s.name == name.as_bytes())
        .unwrap_or_else(|| panic!("no symbol {name}"))
}

// ---------------------------------------------------------------------------
// obj2yaml: the raw fields of a thin Mach-O file
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Item {
    fields: BTreeMap<String, String>,
    children: Vec<Item>,
}

impl Item {
    fn num(&self, key: &str) -> u64 {
        parse_num(
            self.fields
                .get(key)
                .unwrap_or_else(|| panic!("no field {key} in {:?}", self.fields)),
        )
    }
    fn str(&self, key: &str) -> &str {
        self.fields.get(key).map_or("", |s| s.trim_matches('\''))
    }
}

#[derive(Debug, Default)]
struct YamlInventory {
    header: Item,
    commands: Vec<Item>,
    sections: Vec<Item>,
    symbols: Vec<Item>,
    strings: Vec<String>,
    data_in_code: Vec<Item>,
}

impl YamlInventory {
    fn parse(text: &str) -> Self {
        #[derive(Clone, Copy, PartialEq)]
        enum Current {
            Header,
            Command,
            Section,
            Relocation,
            Symbol,
            Dice,
            Other,
        }
        let mut inv = Self::default();
        let mut current = Current::Header;
        let mut strtab_indent = None;
        for line in text.lines() {
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            if trimmed.is_empty() || trimmed.starts_with("---") || trimmed == "..." {
                continue;
            }
            if indent == 0 && trimmed == "DWARF:" {
                // Decoded debug sections follow; nothing qld compares.
                break;
            }
            if let Some(level) = strtab_indent {
                if indent >= level && trimmed.starts_with("- ") {
                    inv.strings
                        .push(trimmed[2..].trim().trim_matches('\'').to_owned());
                    continue;
                }
                strtab_indent = None;
            }
            if trimmed == "StringTable:" {
                strtab_indent = Some(indent);
                continue;
            }
            let (is_item, body) = match trimmed.strip_prefix("- ") {
                Some(rest) => (true, rest),
                None => (false, trimmed),
            };
            let Some((key, value)) = body.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            if is_item {
                current = match key {
                    "cmd" => {
                        inv.commands.push(Item::default());
                        Current::Command
                    }
                    "sectname" => {
                        inv.sections.push(Item::default());
                        Current::Section
                    }
                    "address" => {
                        inv.sections
                            .last_mut()
                            .unwrap()
                            .children
                            .push(Item::default());
                        Current::Relocation
                    }
                    "n_strx" => {
                        inv.symbols.push(Item::default());
                        Current::Symbol
                    }
                    "Offset" => {
                        inv.data_in_code.push(Item::default());
                        Current::Dice
                    }
                    _ => Current::Other,
                };
            }
            if value.is_empty() {
                continue;
            }
            let item = match current {
                Current::Header => &mut inv.header,
                Current::Command => inv.commands.last_mut().unwrap(),
                Current::Section => inv.sections.last_mut().unwrap(),
                Current::Relocation => inv
                    .sections
                    .last_mut()
                    .unwrap()
                    .children
                    .last_mut()
                    .unwrap(),
                Current::Symbol => inv.symbols.last_mut().unwrap(),
                Current::Dice => inv.data_in_code.last_mut().unwrap(),
                Current::Other => continue,
            };
            item.fields.insert(key.to_owned(), value.to_owned());
        }
        inv
    }

    /// The string at `offset` of the string table, which obj2yaml lists as
    /// consecutive NUL-terminated strings.
    fn string_at(&self, offset: u64) -> String {
        let mut start = 0u64;
        for s in &self.strings {
            let end = start + s.len() as u64;
            if offset >= start && offset <= end {
                return s[(offset - start) as usize..].to_owned();
            }
            start = end + 1;
        }
        panic!("string offset {offset} out of range");
    }
}

/// Compares every raw field qld decodes with obj2yaml's dump of `path`.
fn compare_with_obj2yaml(path: &Path) -> bool {
    let Some(text) = run_tool("obj2yaml", &[], path) else {
        return false;
    };
    let yaml = YamlInventory::parse(&text);
    let data = std::fs::read(path).unwrap();
    let object = ObjectFile::parse(&data, src(path)).unwrap();
    let name = path.display();

    let header = object.header();
    assert_eq!(
        u64::from(header.cpu_type),
        yaml.header.num("cputype"),
        "{name}"
    );
    assert_eq!(
        u64::from(header.cpu_subtype),
        yaml.header.num("cpusubtype"),
        "{name}"
    );
    assert_eq!(
        u64::from(header.file_type),
        yaml.header.num("filetype"),
        "{name}"
    );
    assert_eq!(u64::from(header.ncmds), yaml.header.num("ncmds"), "{name}");
    assert_eq!(
        u64::from(header.sizeofcmds),
        yaml.header.num("sizeofcmds"),
        "{name}"
    );
    assert_eq!(u64::from(header.flags), yaml.header.num("flags"), "{name}");

    // Load commands.
    let commands: Vec<_> = object.file().load_commands().map(Result::unwrap).collect();
    assert_eq!(commands.len(), yaml.commands.len(), "{name}: command count");
    for (command, expected) in commands.iter().zip(&yaml.commands) {
        let cmd_name = command.name().unwrap_or("?");
        assert_eq!(cmd_name, expected.str("cmd"), "{name}");
        assert_eq!(
            command.data.len() as u64,
            expected.num("cmdsize"),
            "{name}: {cmd_name}"
        );
        match cmd_name {
            "LC_SYMTAB" => {
                let symtab = command.symtab().unwrap();
                assert_eq!(u64::from(symtab.symoff), expected.num("symoff"));
                assert_eq!(u64::from(symtab.nsyms), expected.num("nsyms"));
                assert_eq!(u64::from(symtab.stroff), expected.num("stroff"));
                assert_eq!(u64::from(symtab.strsize), expected.num("strsize"));
            }
            "LC_DYSYMTAB" => {
                let d = command.dysymtab().unwrap();
                for (value, key) in [
                    (d.ilocalsym, "ilocalsym"),
                    (d.nlocalsym, "nlocalsym"),
                    (d.iextdefsym, "iextdefsym"),
                    (d.nextdefsym, "nextdefsym"),
                    (d.iundefsym, "iundefsym"),
                    (d.nundefsym, "nundefsym"),
                    (d.indirectsymoff, "indirectsymoff"),
                    (d.nindirectsyms, "nindirectsyms"),
                    (d.locreloff, "locreloff"),
                    (d.nlocrel, "nlocrel"),
                ] {
                    assert_eq!(u64::from(value), expected.num(key), "{name}: {key}");
                }
            }
            "LC_BUILD_VERSION" | "LC_VERSION_MIN_MACOSX" => {
                let version = command.build_version().unwrap();
                if cmd_name == "LC_BUILD_VERSION" {
                    assert_eq!(u64::from(version.platform), expected.num("platform"));
                    assert_eq!(u64::from(version.minos.0), expected.num("minos"));
                    assert_eq!(u64::from(version.sdk.0), expected.num("sdk"));
                } else {
                    assert_eq!(u64::from(version.minos.0), expected.num("version"));
                }
                assert_eq!(version.platform, PLATFORM_MACOS);
            }
            "LC_LINKER_OPTION" => {
                let strings = command.linker_option_strings().unwrap();
                assert_eq!(strings.len() as u64, expected.num("count"));
            }
            "LC_DATA_IN_CODE" | "LC_LINKER_OPTIMIZATION_HINT" => {
                let info = command.linkedit_data().unwrap();
                assert_eq!(u64::from(info.dataoff), expected.num("dataoff"));
                assert_eq!(u64::from(info.datasize), expected.num("datasize"));
            }
            "LC_SEGMENT_64" | "LC_SEGMENT" => {
                let segment = command.segment().unwrap();
                assert_eq!(segment.vmsize, expected.num("vmsize"));
                assert_eq!(segment.fileoff, expected.num("fileoff"));
                assert_eq!(segment.filesize, expected.num("filesize"));
                assert_eq!(segment.sections.len() as u64, expected.num("nsects"));
            }
            _ => {}
        }
    }

    // Sections and relocations.
    assert_eq!(
        object.sections().len(),
        yaml.sections.len(),
        "{name}: section count"
    );
    for (index, (section, expected)) in object.sections().iter().zip(&yaml.sections).enumerate() {
        let sname = section.display_name();
        assert_eq!(
            section.sectname,
            expected.str("sectname").as_bytes(),
            "{name}"
        );
        assert_eq!(
            section.segname,
            expected.str("segname").as_bytes(),
            "{name}"
        );
        for (value, key) in [
            (section.addr, "addr"),
            (section.size, "size"),
            (u64::from(section.offset), "offset"),
            (u64::from(section.align), "align"),
            (u64::from(section.reloff), "reloff"),
            (u64::from(section.nreloc), "nreloc"),
            (u64::from(section.flags), "flags"),
            (u64::from(section.reserved1), "reserved1"),
            (u64::from(section.reserved2), "reserved2"),
        ] {
            assert_eq!(value, expected.num(key), "{name}: {sname} {key}");
        }
        let table = object.relocations(index).unwrap();
        assert_eq!(
            table.len(),
            expected.children.len(),
            "{name}: {sname} relocations"
        );
        for (i, (reloc, exp)) in table.iter().zip(&expected.children).enumerate() {
            let what = format!("{name}: {sname} relocation {i}");
            assert_eq!(u64::from(reloc.address), exp.num("address"), "{what}");
            assert_eq!(u64::from(reloc.r_type), exp.num("type"), "{what}");
            assert_eq!(u64::from(reloc.length), exp.num("length"), "{what}");
            assert_eq!(u64::from(reloc.pcrel), exp.num("pcrel"), "{what}");
            assert_eq!(
                u64::from(reloc.is_scattered()),
                exp.num("scattered"),
                "{what}"
            );
            match reloc.target {
                RelocationTarget::Symbol(n) => {
                    assert_eq!(exp.num("extern"), 1, "{what}");
                    assert_eq!(u64::from(n), exp.num("symbolnum"), "{what}");
                }
                RelocationTarget::Section(n) => {
                    assert_eq!(exp.num("extern"), 0, "{what}");
                    assert_eq!(u64::from(n), exp.num("symbolnum"), "{what}");
                }
                RelocationTarget::Scattered(value) => {
                    assert_eq!(u64::from(value), exp.num("value"), "{what}");
                }
            }
            assert!(
                reloc.type_name(header.cpu_type).is_some(),
                "{what}: unnamed type {}",
                reloc.r_type
            );
        }
        // Pairing consumes every entry exactly once.
        let paired: Vec<_> = table
            .paired(object.source())
            .collect::<Result<_, _>>()
            .unwrap();
        let consumed: usize = paired
            .iter()
            .map(|p| p.index - p.first_index + 1 + usize::from(p.pair.is_some()))
            .sum();
        assert_eq!(consumed, table.len(), "{name}: {sname} pairing");
    }

    // Symbols, STABS included.
    let symbols: Vec<_> = object.symbols().iter_all().map(Result::unwrap).collect();
    assert_eq!(symbols.len(), yaml.symbols.len(), "{name}: symbol count");
    for (symbol, expected) in symbols.iter().zip(&yaml.symbols) {
        let what = format!("{name}: symbol {}", symbol.index);
        assert_eq!(u64::from(symbol.n_strx), expected.num("n_strx"), "{what}");
        assert_eq!(u64::from(symbol.n_type), expected.num("n_type"), "{what}");
        assert_eq!(u64::from(symbol.n_sect), expected.num("n_sect"), "{what}");
        assert_eq!(u64::from(symbol.n_desc), expected.num("n_desc"), "{what}");
        assert_eq!(symbol.n_value, expected.num("n_value"), "{what}");
        assert_eq!(
            String::from_utf8_lossy(symbol.name),
            yaml.string_at(u64::from(symbol.n_strx)),
            "{what}"
        );
    }

    // Data in code.
    let dice: Vec<_> = object.data_in_code().collect();
    assert_eq!(dice.len(), yaml.data_in_code.len(), "{name}: data in code");
    for (entry, expected) in dice.iter().zip(&yaml.data_in_code) {
        assert_eq!(u64::from(entry.offset), expected.num("Offset"));
        assert_eq!(u64::from(entry.length), expected.num("Length"));
        assert_eq!(u64::from(entry.kind), expected.num("Kind"));
    }
    true
}

// ---------------------------------------------------------------------------
// Atomization invariants
// ---------------------------------------------------------------------------

fn check_atoms(object: &ObjectFile<'_>, atoms: &Atomization, name: &str) {
    for (index, section) in object.sections().iter().enumerate() {
        let list = atoms.section_atoms(index);
        let sname = section.display_name();
        let mut end = 0;
        for atom in list {
            assert_eq!(atom.offset, end, "{name}: {sname} atoms are not contiguous");
            assert!(
                atom.align <= section.align,
                "{name}: {sname} atom alignment"
            );
            end = atom.offset + atom.size;
        }
        if !list.is_empty() {
            assert_eq!(
                end, section.size,
                "{name}: {sname} atoms do not cover the section"
            );
        }
        // Every atom boundary of a regular split section is a symbol.
        if object.subsections_via_symbols() {
            let range = atoms.section_range(index).unwrap();
            for atom_index in range {
                let atom = &atoms.atoms()[atom_index];
                if atom.kind != AtomKind::Regular || atom.offset == 0 {
                    continue;
                }
                let starts = atoms.atom_symbols(atom_index).iter().any(|&s| {
                    let symbol = object.symbols().get(s).unwrap();
                    symbol.n_value - section.addr == atom.offset && !symbol.is_alt_entry()
                });
                assert!(
                    starts,
                    "{name}: {sname} atom at {:#x} has no symbol",
                    atom.offset
                );
            }
        }
        // Relocations all land in atoms.
        let mapped = atoms.relocations(object, index).unwrap();
        for m in &mapped {
            let atom = &atoms.atoms()[m.atom];
            assert_eq!(atom.section as usize, index);
            assert!(
                atom.range()
                    .contains(&u64::from(m.relocation.relocation.address))
            );
        }
    }
    for symbol in object.symbols().iter().map(Result::unwrap) {
        if symbol.n_type & consts::N_TYPE != consts::N_SECT {
            continue;
        }
        let atom = atoms
            .symbol_atom(symbol.index)
            .unwrap_or_else(|| panic!("{name}: symbol {} has no atom", symbol.index));
        let atom = &atoms.atoms()[atom];
        let section = &object.sections()[atom.section as usize];
        let offset = symbol.n_value - section.addr;
        assert!(
            atom.range().contains(&offset) || offset == atom.offset + atom.size,
            "{name}: symbol {} outside its atom",
            String::from_utf8_lossy(symbol.name)
        );
    }
}

fn atom_names(object: &ObjectFile<'_>, atoms: &Atomization, section: usize) -> Vec<String> {
    let range = atoms.section_range(section).unwrap();
    range
        .map(|i| {
            let atom = &atoms.atoms()[i];
            let names: Vec<_> = atoms
                .atom_symbols(i)
                .iter()
                .map(|&s| {
                    String::from_utf8_lossy(object.symbols().get(s).unwrap().name).into_owned()
                })
                .collect();
            format!("{:#x}+{:#x}:{}", atom.offset, atom.size, names.join(","))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

const OBJECT_FIXTURES: &[&str] = &[
    "atoms-arm64.o",
    "atoms-x86_64.o",
    "alt-entry-arm64.o",
    "eh-arm64.o",
    "eh-dwarf-x86_64.o",
    "i386.o",
];

#[test]
fn fixtures_match_obj2yaml() {
    for name in OBJECT_FIXTURES {
        if !compare_with_obj2yaml(&data_dir().join(name)) {
            return;
        }
    }
}

#[test]
fn fresh_objects_match_obj2yaml() {
    let builds: &[(&str, &[&str], &str, &str)] = &[
        (
            "clang",
            &["--target=arm64-apple-macos13", "-O1", "-fcommon"],
            "atoms.c",
            "atoms-arm64.o",
        ),
        (
            "clang",
            &["--target=x86_64-apple-macos13", "-O2", "-fcommon"],
            "atoms.c",
            "atoms-x86_64.o",
        ),
        (
            "clang",
            &["--target=arm64-apple-macos13"],
            "alt_entry_arm64.s",
            "alt-arm64.o",
        ),
        (
            "clang++",
            &["--target=arm64-apple-macos13", "-O1"],
            "eh.cpp",
            "eh-arm64.o",
        ),
        (
            "clang++",
            &["--target=x86_64-apple-macos13", "-O0"],
            "eh.cpp",
            "eh-x86_64.o",
        ),
        (
            "clang++",
            &[
                "--target=arm64-apple-macos13",
                "-O1",
                "-femit-dwarf-unwind=always",
            ],
            "eh.cpp",
            "eh-dwarf-arm64.o",
        ),
        (
            "clang",
            &["--target=i386-apple-macos10.14", "-O1"],
            "i386.c",
            "i386.o",
        ),
        (
            "clang",
            &["--target=arm64-apple-macos13", "-O2", "-g"],
            "atoms.c",
            "atoms-debug-arm64.o",
        ),
    ];
    for (compiler, args, source, out) in builds {
        let Some(path) = compile(compiler, args, source, out) else {
            continue;
        };
        if !compare_with_obj2yaml(&path) {
            return;
        }
        let data = std::fs::read(&path).unwrap();
        let object = ObjectFile::parse(&data, src(&path)).unwrap();
        let atoms = Atomization::new(&object).unwrap();
        check_atoms(&object, &atoms, out);
        compact_unwind_entries(&object).unwrap();
        EhFrame::from_object(&object).unwrap();
        compare_unwind_with_objdump(&path, &object);
        compare_eh_frame_with_dwarfdump(&path, &object);
    }
}

#[test]
fn atomization_via_symbols() {
    let data = fixture("alt-entry-arm64.o");
    let path = data_dir().join("alt-entry-arm64.o");
    let object = ObjectFile::parse(&data, src(&path)).unwrap();
    assert!(object.subsections_via_symbols());
    let atoms = Atomization::new(&object).unwrap();
    check_atoms(&object, &atoms, "alt-entry-arm64.o");

    // _inner is an alternate entry into _outer's atom.
    assert_eq!(
        atom_names(&object, &atoms, 0),
        [
            "0x0+0xc:ltmp0,_outer,_inner",
            "0xc+0x10:_table_user",
            "0x1c+0x14:_after_table"
        ]
    );
    let inner = find_symbol(&object, "_inner");
    assert!(inner.is_alt_entry() && inner.is_external());

    // Relocations map to the atoms holding their fixups, pairs folded.
    let mapped = atoms.relocations(&object, 0).unwrap();
    let summary: Vec<_> = mapped
        .iter()
        .map(|m| {
            let r = &m.relocation;
            let kind = r.relocation.type_name(CPU_TYPE_ARM64).unwrap();
            format!(
                "{}@{:#x}{}{}",
                kind.trim_start_matches("ARM64_RELOC_"),
                r.relocation.address,
                r.addend.map_or(String::new(), |a| format!("+{a}")),
                r.subtractor
                    .map_or(String::new(), |s| format!("-sym{}", s.symbol().unwrap())),
            ) + &format!("/atom{}", m.atom)
        })
        .collect();
    assert_eq!(
        summary,
        [
            "GOT_LOAD_PAGEOFF12@0x28/atom2",
            "GOT_LOAD_PAGE21@0x24/atom2",
            "PAGEOFF12@0x20+8/atom2",
            "PAGE21@0x1c/atom2",
            "UNSIGNED@0x18-sym1/atom1",
            "UNSIGNED@0x14-sym1/atom1",
            "BRANCH26@0x8/atom0",
        ]
    );
    let raw = object.relocations(0).unwrap();
    assert_eq!(raw.get(2).unwrap().r_type, ARM64_RELOC_ADDEND);
    assert_eq!(raw.get(3).unwrap().r_type, ARM64_RELOC_PAGEOFF12);
    assert_eq!(raw.get(5).unwrap().r_type, ARM64_RELOC_SUBTRACTOR);

    // Literal and special sections.
    let (literal16, section) = object.find_section(b"__TEXT", b"__literal16").unwrap();
    assert_eq!(
        section.literal_kind(),
        qld::macho::read::LiteralKind::Fixed(16)
    );
    let list = atoms.section_atoms(literal16);
    assert_eq!(list.len(), 2);
    assert!(
        list.iter()
            .all(|a| a.kind == AtomKind::Literal && a.size == 16)
    );
    let (_, keep) = object.find_section(b"__DATA", b"__keep").unwrap();
    assert!(keep.is_no_dead_strip());
    assert_eq!(keep.attributes(), S_ATTR_NO_DEAD_STRIP);
    let (_, init) = object.find_section(b"__DATA", b"__mod_init_func").unwrap();
    assert_eq!(init.section_type(), consts::S_MOD_INIT_FUNC_POINTERS);

    // Data in code, linker options.
    let dice: Vec<_> = object.data_in_code().collect();
    assert_eq!(dice.len(), 1);
    assert_eq!(
        (dice[0].offset, dice[0].length, dice[0].kind),
        (0x14, 8, DICE_KIND_JUMP_TABLE32)
    );
    assert_eq!(
        object.linker_option_hints().unwrap(),
        [
            LinkerOptionHint::Framework(b"QldKit"),
            LinkerOptionHint::Library(b"qldasm")
        ]
    );
    assert_eq!(
        object.build_version().unwrap().minos,
        PackedVersion::new(13, 0, 0)
    );
}

#[test]
fn atomization_of_c_object() {
    for (name, cpu) in [
        ("atoms-arm64.o", CPU_TYPE_ARM64),
        ("atoms-x86_64.o", CPU_TYPE_X86_64),
    ] {
        let data = fixture(name);
        let path = data_dir().join(name);
        let object = ObjectFile::parse(&data, src(&path)).unwrap();
        assert_eq!(object.header().cpu_type, cpu);
        let atoms = Atomization::new(&object).unwrap();
        check_atoms(&object, &atoms, name);

        // C strings split at NULs, symbols attached to their pieces.
        let (cstring, _) = object
            .find_section(b"__TEXT", b"__cstring")
            .unwrap_or_else(|| {
                // x86_64 at -O1 folded the strings into __const.
                object.find_section(b"__TEXT", b"__const").unwrap()
            });
        let pieces: Vec<_> = atoms
            .section_atoms(cstring)
            .iter()
            .map(|a| {
                object.section_data(cstring).unwrap()
                    [a.range().start as usize..a.range().end as usize]
                    .to_vec()
            })
            .collect();
        if object.sections()[cstring].literal_kind() == qld::macho::read::LiteralKind::CString {
            assert_eq!(
                pieces,
                [b"alpha\0".to_vec(), b"beta\0".to_vec(), b"gamma\0".to_vec()]
            );
        }

        // Compact unwind: one atom per record.
        let (unwind, section) = object.find_section(b"__LD", b"__compact_unwind").unwrap();
        let records = atoms.section_atoms(unwind);
        assert_eq!(records.len() as u64, section.size / 32);
        assert!(records.iter().all(|a| a.kind == AtomKind::CompactUnwind));
        let entries = compact_unwind_entries(&object).unwrap();
        assert_eq!(entries.len(), records.len());
        for entry in &entries {
            let function = entry.function.relocation.expect("function relocation");
            assert!(
                !function.relocation.is_extern(),
                "{name}: section-relative function"
            );
            assert!(entry.length > 0);
        }

        // Symbol flags.
        let common = find_symbol(&object, "_common_var");
        assert!(common.is_common() && !common.is_undefined());
        assert_eq!((common.n_value, common.common_align()), (4, 2));
        let array = find_symbol(&object, "_common_array");
        assert_eq!(array.n_value, 256);
        assert!(
            array.common_align() >= 2,
            "{name}: {}",
            array.common_align()
        );
        assert!(find_symbol(&object, "_weak_function").is_weak_def());
        assert!(find_symbol(&object, "_weak_var").is_weak_def());
        assert!(find_symbol(&object, "_kept").is_no_dead_strip());
        assert!(find_symbol(&object, "_cold_function").is_cold_func());
        assert!(find_symbol(&object, "_strlen").is_undefined() || cpu == CPU_TYPE_X86_64);

        // Thread-local variables.
        let types: Vec<_> = object.sections().iter().map(|s| s.section_type()).collect();
        assert!(types.contains(&S_THREAD_LOCAL_VARIABLES));
        assert!(types.contains(&S_THREAD_LOCAL_REGULAR));
        assert!(types.contains(&S_THREAD_LOCAL_ZEROFILL));
        let (bss, section) = object.find_section(b"__DATA", b"__thread_bss").unwrap();
        assert!(section.is_zerofill());
        assert!(object.section_data(bss).unwrap().is_empty());
        let (vars, section) = object.find_section(b"__DATA", b"__thread_vars").unwrap();
        // Two descriptors of three words each, split at their symbols.
        assert_eq!(section.size, 0x30);
        assert_eq!(atoms.section_atoms(vars).len(), 2);
        let tlv_type = if cpu == CPU_TYPE_ARM64 {
            ARM64_RELOC_TLVP_LOAD_PAGE21
        } else {
            X86_64_RELOC_TLV
        };
        let text = object.relocations(0).unwrap();
        assert!(
            text.iter().any(|r| r.r_type == tlv_type),
            "{name}: TLV relocation"
        );
    }

    // x86_64 fixed-size literals.
    let data = fixture("atoms-x86_64.o");
    let path = data_dir().join("atoms-x86_64.o");
    let object = ObjectFile::parse(&data, src(&path)).unwrap();
    let atoms = Atomization::new(&object).unwrap();
    for (sectname, size) in [(&b"__literal8"[..], 8), (b"__literal4", 4)] {
        let (index, section) = object.find_section(b"__TEXT", sectname).unwrap();
        assert_eq!(
            section.literal_kind(),
            qld::macho::read::LiteralKind::Fixed(size)
        );
        let list = atoms.section_atoms(index);
        assert!(!list.is_empty() && list.iter().all(|a| a.size == u64::from(size)));
    }
    // An arm64 object carries linker optimization hints at -O1.
    let data = fixture("atoms-arm64.o");
    let path = data_dir().join("atoms-arm64.o");
    let object = ObjectFile::parse(&data, src(&path)).unwrap();
    assert!(!object.optimization_hint_bytes().is_empty());
    let hints: Vec<_> = object
        .optimization_hints()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!hints.is_empty());
    for hint in &hints {
        assert!((1..=8).contains(&hint.kind), "LOH kind {}", hint.kind);
        assert!(hint.addresses().len() >= 2);
    }
}

#[test]
fn i386_scattered_relocations() {
    let data = fixture("i386.o");
    let path = data_dir().join("i386.o");
    let object = ObjectFile::parse(&data, src(&path)).unwrap();
    assert_eq!(object.header().cpu_type, CPU_TYPE_X86);
    assert!(!object.file().is64());
    let table = object.relocations(0).unwrap();
    let paired: Vec<_> = table
        .paired(object.source())
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!paired.is_empty());
    for p in &paired {
        assert!(p.relocation.is_scattered());
        let pair = p.pair.expect("SECTDIFF has a PAIR");
        assert_eq!(pair.r_type, GENERIC_RELOC_PAIR);
    }
    let atoms = Atomization::new(&object).unwrap();
    check_atoms(&object, &atoms, "i386.o");
    // 20-byte compact unwind records on 32-bit.
    let entries = compact_unwind_entries(&object).unwrap();
    let (_, section) = object.find_section(b"__LD", b"__compact_unwind").unwrap();
    assert_eq!(entries.len() as u64, section.size / 20);
}

// ---------------------------------------------------------------------------
// Compact unwind and __eh_frame
// ---------------------------------------------------------------------------

fn symbol_name(object: &ObjectFile<'_>, index: u32) -> String {
    String::from_utf8_lossy(object.symbols().get(index).unwrap().name).into_owned()
}

/// Compares `__compact_unwind` entries with `llvm-objdump --unwind-info`.
fn compare_unwind_with_objdump(path: &Path, object: &ObjectFile<'_>) {
    let Some(text) = run_tool("llvm-objdump", &["--macho", "--unwind-info"], path) else {
        return;
    };
    let mut expected = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Entry at offset ") {
            expected.push(format!("entry {}", rest.trim_end_matches(':')));
        } else if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            let first = value.split_whitespace().next().unwrap_or("");
            match key {
                "start" | "length" | "compact encoding" => {
                    expected.push(format!("{key} {:#x}", parse_num(first)));
                }
                "personality function" | "LSDA" => {
                    expected.push(format!("{key} {:#x}", parse_num(first)));
                }
                _ => {}
            }
        }
    }
    let mut actual = Vec::new();
    for entry in compact_unwind_entries(object).unwrap() {
        actual.push(format!("entry {:#x}", entry.offset));
        actual.push(format!("start {:#x}", entry.function.value));
        actual.push(format!("length {:#x}", entry.length));
        actual.push(format!("compact encoding {:#x}", entry.encoding));
        if entry.personality.value != 0 || entry.personality.relocation.is_some() {
            actual.push(format!(
                "personality function {:#x}",
                entry.personality.value
            ));
            if let Some(r) = entry.personality.relocation {
                assert!(
                    symbol_name(object, r.relocation.symbol().unwrap()).contains("personality")
                );
            }
        }
        if entry.lsda.value != 0 || entry.lsda.relocation.is_some() {
            actual.push(format!("LSDA {:#x}", entry.lsda.value));
        }
    }
    assert_eq!(actual, expected, "{}", path.display());
}

/// Compares `__eh_frame` records with `llvm-dwarfdump --eh-frame`.
fn compare_eh_frame_with_dwarfdump(path: &Path, object: &ObjectFile<'_>) {
    let Some(frame) = EhFrame::from_object(object).unwrap() else {
        return;
    };
    let Some(text) = run_tool("llvm-dwarfdump", &["--eh-frame"], path) else {
        return;
    };
    let mut expected = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 4
            && fields[0].len() == 8
            && fields[..3]
                .iter()
                .all(|f| f.chars().all(|c| c.is_ascii_hexdigit()))
            && (fields[3] == "CIE" || fields[3] == "FDE")
        {
            let mut entry = format!("{} {} {}", fields[0], fields[1], fields[3]);
            if fields[3] == "FDE" {
                let cie = fields[4].trim_start_matches("cie=");
                let pc = fields[5].trim_start_matches("pc=");
                let _ = write!(entry, " cie={cie} pc={pc}");
            }
            expected.push(entry);
        }
    }
    let mut actual = Vec::new();
    for record in &frame.records {
        let length = record.data.len() - record.length_size;
        match record.kind {
            EhFrameKind::Cie(_) => actual.push(format!("{:08x} {length:08x} CIE", record.offset)),
            EhFrameKind::Fde(fde) => {
                let begin = fde.pc_begin.address;
                actual.push(format!(
                    "{:08x} {length:08x} FDE cie={:08x} pc={begin:08x}...{:08x}",
                    record.offset,
                    fde.cie_offset,
                    begin + fde.pc_range
                ));
            }
        }
    }
    assert_eq!(actual, expected, "{}", path.display());
}

#[test]
fn compact_unwind_and_eh_frame() {
    let data = fixture("eh-arm64.o");
    let path = data_dir().join("eh-arm64.o");
    let object = ObjectFile::parse(&data, src(&path)).unwrap();
    let entries = compact_unwind_entries(&object).unwrap();
    assert_eq!(entries.len(), 4);
    let with_lsda: Vec<_> = entries.iter().filter(|e| e.has_lsda()).collect();
    assert_eq!(with_lsda.len(), 2);
    for entry in &with_lsda {
        let personality = entry.personality.relocation.unwrap();
        assert_eq!(
            symbol_name(&object, personality.relocation.symbol().unwrap()),
            "___gxx_personality_v0"
        );
        let lsda = entry.lsda.relocation.unwrap();
        assert!(!lsda.relocation.is_extern());
        let section = object.section_by_ordinal(match lsda.relocation.target {
            RelocationTarget::Section(n) => n,
            _ => unreachable!(),
        });
        assert_eq!(section.unwrap().sectname, b"__gcc_except_tab");
        assert!(!entry.needs_dwarf(CPU_TYPE_ARM64));
    }
    assert!(EhFrame::from_object(&object).unwrap().is_none());
    compare_unwind_with_objdump(&path, &object);

    let data = fixture("eh-dwarf-x86_64.o");
    let path = data_dir().join("eh-dwarf-x86_64.o");
    let object = ObjectFile::parse(&data, src(&path)).unwrap();
    let frame = EhFrame::from_object(&object).unwrap().unwrap();
    let cies = frame
        .records
        .iter()
        .filter(|r| matches!(r.kind, EhFrameKind::Cie(_)))
        .count();
    let fdes = frame.records.len() - cies;
    assert!(cies >= 1 && fdes == 4, "{cies} CIEs, {fdes} FDEs");
    // Every FDE starts at a function symbol.
    let atoms = Atomization::new(&object).unwrap();
    check_atoms(&object, &atoms, "eh-dwarf-x86_64.o");
    for record in &frame.records {
        if let EhFrameKind::Fde(fde) = record.kind {
            let text = object.section_at_address(fde.pc_begin.address).unwrap();
            let offset = fde.pc_begin.address - object.sections()[text].addr;
            let atom = atoms.atom_at(text, offset).unwrap();
            assert_eq!(atoms.atoms()[atom].offset, offset);
            if let EhFrameKind::Cie(cie) = frame.records[fde.cie_index].kind
                && cie.lsda_encoding.is_some()
            {
                assert!(fde.lsda.is_some());
            }
        }
    }
    compare_unwind_with_objdump(&path, &object);
    compare_eh_frame_with_dwarfdump(&path, &object);
    let path = data_dir().join("atoms-x86_64.o");
    let data = fixture("atoms-x86_64.o");
    let object = ObjectFile::parse(&data, src(&path)).unwrap();
    compare_eh_frame_with_dwarfdump(&path, &object);
}

// ---------------------------------------------------------------------------
// Universal binaries and archives
// ---------------------------------------------------------------------------

#[test]
fn fat_objects_and_archives() {
    let data = fixture("atoms-fat.o");
    let path = data_dir().join("atoms-fat.o");
    assert!(FatFile::is_fat(&data));
    let fat = FatFile::parse(&data, src(&path)).unwrap();
    assert_eq!(fat.slices().len(), 2);
    for (arch, thin) in [
        (Arch::ARM64, "atoms-arm64.o"),
        (Arch::X86_64, "atoms-x86_64.o"),
    ] {
        let slice = fat.select(arch).unwrap();
        assert_eq!(slice.data, fixture(thin).as_slice());
        let object = ObjectFile::parse(slice.data, fat.slice_source(slice)).unwrap();
        assert_eq!(object.header().arch(), arch);
    }
    let error = fat.select(Arch::I386).unwrap_err().to_string();
    assert!(error.contains("available: x86_64, arm64"), "{error}");

    if let Some(text) = run_tool("obj2yaml", &[], &path) {
        let archs: Vec<_> = text
            .lines()
            .skip_while(|l| !l.starts_with("FatArchs:"))
            .take_while(|l| !l.starts_with("Slices:"))
            .filter_map(|l| l.trim().trim_start_matches("- ").split_once(':'))
            .filter(|(_, v)| !v.trim().is_empty())
            .map(|(k, v)| (k.trim().to_owned(), parse_num(v)))
            .collect();
        let mut actual = Vec::new();
        for slice in fat.slices() {
            actual.push(("cputype".to_owned(), u64::from(slice.arch.cpu_type)));
            actual.push(("cpusubtype".to_owned(), u64::from(slice.raw_cpu_subtype)));
            actual.push(("offset".to_owned(), slice.offset));
            actual.push(("size".to_owned(), slice.size));
            actual.push(("align".to_owned(), u64::from(slice.align)));
        }
        assert_eq!(actual, archs);
    }

    // A universal archive: select the slice, then read the BSD archive.
    let data = fixture("libatoms-fat.a");
    let path = data_dir().join("libatoms-fat.a");
    let fat = FatFile::parse(&data, src(&path)).unwrap();
    let slice = fat.select(Arch::ARM64).unwrap();
    let archive = Archive::parse(&path, slice.data).unwrap();
    assert!(archive.symbol_index().is_some());
    let mut names = Vec::new();
    for member in archive.members() {
        let member = member.unwrap();
        let name = member.display_name();
        let object =
            ObjectFile::parse(member.bytes().unwrap(), Source::member(&path, &name)).unwrap();
        assert_eq!(object.header().arch(), Arch::ARM64);
        Atomization::new(&object).unwrap();
        names.push(name);
    }
    assert_eq!(names, ["atoms-arm64.o", "alt-entry-arm64.o"]);
    assert_eq!(fat.select(Arch::X86_64).unwrap().arch, Arch::X86_64);
}

// ---------------------------------------------------------------------------
// Dylibs
// ---------------------------------------------------------------------------

fn short_name(install_name: &[u8]) -> String {
    let name = String::from_utf8_lossy(install_name);
    let base = name.rsplit('/').next().unwrap_or(&name);
    base.split('.').next().unwrap_or(base).to_owned()
}

fn render_version(v: PackedVersion) -> String {
    format!("{}.{}.{}", v.major(), v.minor(), v.patch())
}

#[test]
fn dylib_with_dyld_info_trie() {
    let data = fixture("libqld-arm64.dylib");
    let path = data_dir().join("libqld-arm64.dylib");
    let dylib = Dylib::parse(&data, src(&path)).unwrap();
    assert_eq!(dylib.header().file_type, MH_DYLIB);
    let id = dylib.id().unwrap();
    assert_eq!(id.name, b"@rpath/libqld.dylib");
    assert_eq!(id.current_version, PackedVersion::new(1, 2, 3));
    assert_eq!(id.compatibility_version, PackedVersion::new(1, 0, 0));
    let kinds: Vec<_> = dylib
        .dependencies()
        .iter()
        .map(|d| (d.kind, short_name(d.name)))
        .collect();
    assert_eq!(
        kinds,
        [
            (DylibLoadKind::Regular, "libSystem".to_owned()),
            (DylibLoadKind::Weak, "libweak".to_owned()),
            (DylibLoadKind::Reexport, "libinner".to_owned()),
            (DylibLoadKind::Upward, "libupward".to_owned()),
        ]
    );
    assert_eq!(dylib.reexports().count(), 1);
    assert_eq!(dylib.rpaths(), [&b"@loader_path/lib"[..], b"/opt/qld/lib"]);
    assert_eq!(dylib.umbrella(), Some(&b"QldKit"[..]));
    assert_eq!(dylib.allowable_clients(), [&b"QldApp"[..]]);
    let version = dylib.build_versions()[0];
    assert_eq!(
        (version.platform, version.minos, version.sdk),
        (
            PLATFORM_MACOS,
            PackedVersion::new(13, 0, 0),
            PackedVersion::new(13, 1, 0)
        )
    );
    assert_eq!(
        version.tools(dylib.file().endian()).collect::<Vec<_>>(),
        [(consts::TOOL_LD, PackedVersion(71_582_720))]
    );
    assert!(dylib.chained_fixups().unwrap().is_none());

    let exports: Vec<_> = dylib.exports().collect::<Result<_, _>>().unwrap();
    let rendered = render_exports(&dylib, &exports);
    assert_eq!(
        rendered,
        [
            "0x00003F40  _qld_one",
            "0x00003F48  _qld_two [weak_def]",
            "[re-export] _qld_again (_inner_again from libinner)",
            "0x00004000  _qld_tlv [per-thread]",
            "0x00003F48  _qld_ifunc [resolver=0x00003F4C]",
        ]
    );
    if let Some(text) = run_tool("llvm-objdump", &["--macho", "--exports-trie"], &path) {
        let expected: Vec<_> = text
            .lines()
            .skip_while(|l| !l.starts_with("Exports trie:"))
            .skip(1)
            .map(str::to_owned)
            .collect();
        assert_eq!(rendered, expected);
    }
    if let Some(text) = run_tool("llvm-objdump", &["--macho", "--dylibs-used"], &path) {
        let expected: Vec<_> = text.lines().skip(1).map(|l| l.trim().to_owned()).collect();
        let mut actual = Vec::new();
        // --dylibs-used lists LC_ID_DYLIB first for dylibs.
        actual.push(format!(
            "{} (compatibility version {}, current version {})",
            String::from_utf8_lossy(id.name),
            render_version(id.compatibility_version),
            render_version(id.current_version)
        ));
        for d in dylib.dependencies() {
            let suffix = match d.kind {
                DylibLoadKind::Weak => ", weak",
                DylibLoadKind::Reexport => ", reexport",
                DylibLoadKind::Upward => ", upward",
                DylibLoadKind::Lazy => ", lazy",
                DylibLoadKind::Regular => "",
            };
            actual.push(format!(
                "{} (compatibility version {}, current version {}{suffix})",
                String::from_utf8_lossy(d.name),
                render_version(d.compatibility_version),
                render_version(d.current_version)
            ));
        }
        assert_eq!(actual, expected);
    }

    // The nlist symbols of the same dylib.
    let symbols: Vec<_> = dylib.symbols().iter().map(Result::unwrap).collect();
    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[1].name, b"_qld_two");
    assert!(symbols[1].is_weak_def());
}

fn render_exports(dylib: &Dylib<'_>, exports: &[qld::macho::read::Export<'_>]) -> Vec<String> {
    exports
        .iter()
        .map(|e| {
            let name = String::from_utf8_lossy(&e.name);
            match &e.target {
                ExportTarget::Address(address) => {
                    let mut s = format!("0x{address:08X}  {name}");
                    if e.is_weak() {
                        s.push_str(" [weak_def]");
                    }
                    if e.is_thread_local() {
                        s.push_str(" [per-thread]");
                    }
                    s
                }
                ExportTarget::Reexport {
                    ordinal,
                    name: imported,
                } => {
                    let lib = short_name(dylib.dependencies()[*ordinal as usize - 1].name);
                    if imported.is_empty() {
                        format!("[re-export] {name} (from {lib})")
                    } else {
                        format!(
                            "[re-export] {name} ({} from {lib})",
                            String::from_utf8_lossy(imported)
                        )
                    }
                }
                ExportTarget::StubAndResolver { stub, resolver } => {
                    format!("0x{stub:08X}  {name} [resolver=0x{resolver:08X}]")
                }
            }
        })
        .collect()
}

#[test]
fn dylib_with_chained_fixups() {
    let data = fixture("libchained-x86_64.dylib");
    let path = data_dir().join("libchained-x86_64.dylib");
    let dylib = Dylib::parse(&data, src(&path)).unwrap();
    assert_eq!(dylib.header().arch(), Arch::X86_64);
    assert_eq!(dylib.install_name(), b"/usr/lib/libchained.dylib");
    let exports: Vec<_> = dylib.exports().collect::<Result<_, _>>().unwrap();
    assert_eq!(render_exports(&dylib, &exports), ["0x00000FF0  _chained"]);
    let fixups = dylib.chained_fixups().unwrap().unwrap();
    assert_eq!(fixups.header.imports_count, 3);
    let imports: Vec<_> = fixups.imports().collect::<Result<_, _>>().unwrap();
    let rendered: Vec<_> = imports
        .iter()
        .map(|i| {
            format!(
                "{} {} {}",
                i.lib_ordinal,
                u8::from(i.weak_import),
                String::from_utf8_lossy(i.name)
            )
        })
        .collect();
    assert_eq!(rendered, ["1 0 _malloc", "1 0 _free", "-2 1 _weak_thing"]);
    if let Some(text) = run_tool("llvm-objdump", &["--macho", "--chained-fixups"], &path) {
        let mut expected = Vec::new();
        let mut current = (String::new(), String::new());
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("lib_ordinal = ") {
                current.0 = v.split_whitespace().next().unwrap().to_owned();
            } else if let Some(v) = line.strip_prefix("weak_import = ") {
                current.1 = v.to_owned();
            } else if let Some(v) = line.strip_prefix("name_offset = ") {
                let name = v.split('(').nth(1).unwrap().trim_end_matches(')');
                expected.push(format!("{} {} {name}", current.0, current.1));
            }
        }
        assert_eq!(rendered, expected);
    }
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Runs every reader over `data`, ignoring errors. Panics are failures.
fn exercise(data: &[u8], depth: u32) {
    let path = Path::new("fuzz");
    let source = Source::new(path);
    if depth > 2 {
        return;
    }
    if FatFile::is_fat(data) {
        if let Ok(fat) = FatFile::parse(data, source) {
            for slice in fat.slices() {
                exercise(slice.data, depth + 1);
            }
            let _ = fat.select(Arch::ARM64);
        }
        return;
    }
    if data.starts_with(b"!<arch>\n") {
        if let Ok(archive) = Archive::parse(path, data) {
            for member in archive.members().take(16) {
                if let Ok(member) = member
                    && let Some(bytes) = member.bytes()
                {
                    exercise(bytes, depth + 1);
                }
            }
        }
        return;
    }
    if let Ok(object) = ObjectFile::parse(data, source) {
        for symbol in object.symbols().iter_all().flatten() {
            let _ = symbol.is_common();
            if symbol.kind() == qld::macho::read::SymbolKind::Indirect {
                let _ = object.symbols().indirect_name(&symbol);
            }
        }
        for index in 0..object.sections().len() {
            let _ = object.section_data(index);
            if let Ok(table) = object.relocations(index) {
                for r in table.paired(object.source()) {
                    let _ = r;
                }
            }
        }
        if let Ok(atoms) = Atomization::new(&object) {
            for index in 0..object.sections().len() {
                let _ = atoms.relocations(&object, index);
            }
        }
        let _ = compact_unwind_entries(&object);
        let _ = EhFrame::from_object(&object);
        let _ = object.linker_option_hints();
        let _ = object.data_in_code().count();
        let _ = object.optimization_hints().count();
    }
    if let Ok(dylib) = Dylib::parse(data, source) {
        let _ = dylib.exports().take(10_000).count();
        if let Ok(Some(fixups)) = dylib.chained_fixups() {
            let _ = fixups.imports().take(10_000).count();
        }
        let _ = dylib.symbols().iter().count();
    }
}

#[test]
fn truncated_and_corrupted_inputs_never_panic() {
    let names = [
        "atoms-arm64.o",
        "atoms-x86_64.o",
        "alt-entry-arm64.o",
        "eh-arm64.o",
        "eh-dwarf-x86_64.o",
        "i386.o",
        "atoms-fat.o",
        "libatoms-fat.a",
        "libqld-arm64.dylib",
        "libchained-x86_64.dylib",
    ];
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    for name in names {
        let data = fixture(name);
        exercise(&data, 0);
        // Truncations.
        let step = (data.len() / 400).max(1);
        for len in (0..data.len()).step_by(step) {
            exercise(&data[..len], 0);
        }
        // Byte flips, concentrated in the headers and load commands where
        // most offsets live.
        for round in 0..400 {
            let mut copy = data.clone();
            let flips = 1 + rng.below(8);
            for _ in 0..flips {
                let limit = if round % 2 == 0 {
                    copy.len().min(1400)
                } else {
                    copy.len()
                };
                let at = rng.below(limit);
                copy[at] = match rng.below(4) {
                    0 => 0xff,
                    1 => 0,
                    _ => rng.next() as u8,
                };
            }
            exercise(&copy, 0);
        }
    }
}

#[test]
fn export_trie_and_chained_fixups_corruption_never_panics() {
    let mut rng = Rng(0x7e1e_0001_0002_0003);
    for name in ["libqld-arm64.dylib", "libchained-x86_64.dylib"] {
        let data = fixture(name);
        let path = data_dir().join(name);
        let dylib = Dylib::parse(&data, src(&path)).unwrap();
        let trie = dylib.exports_trie().to_vec();
        let fixups = dylib
            .chained_fixups()
            .unwrap()
            .map(|f| data[f.file_offset() as usize..][..f.size() as usize].to_vec());
        for round in 0..3000 {
            let mut copy = trie.clone();
            if round % 3 == 0 {
                copy.truncate(rng.below(copy.len() + 1));
            }
            for _ in 0..1 + rng.below(4) {
                if !copy.is_empty() {
                    let at = rng.below(copy.len());
                    copy[at] = rng.next() as u8;
                }
            }
            let _ = qld::macho::read::ExportTrieIter::new(&copy, 0, src(&path)).count();
            if let Some(blob) = &fixups {
                let mut copy = blob.clone();
                let at = rng.below(copy.len());
                copy[at] = rng.next() as u8;
                if round % 5 == 0 {
                    copy.truncate(rng.below(copy.len() + 1));
                }
                if let Ok(f) = qld::macho::read::ChainedFixups::parse(
                    &copy,
                    0,
                    qld::macho::read::Endian::LITTLE,
                    src(&path),
                ) {
                    let _ = f.imports().take(100_000).count();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Text-based stubs
// ---------------------------------------------------------------------------

fn stub(name: &str) -> TextStub {
    let path = data_dir().join(name);
    TextStub::parse(&fixture(name), src(&path)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn target(text: &str) -> StubTarget {
    StubTarget::parse(text).unwrap()
}

/// Everything a link needs from one library, for every target, in a form
/// that does not depend on how the file grouped it.
fn stub_view(lib: &StubLibrary) -> Vec<String> {
    let mut targets = lib.targets.clone();
    targets.sort();
    let mut out = vec![
        format!("install-name {}", lib.install_name),
        format!("current {}", lib.current_version),
        format!("compatibility {}", lib.compatibility_version),
        format!("swift {}", lib.swift_abi_version),
        format!("flags {:?}", lib.flags),
        format!(
            "targets {}",
            targets
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        ),
    ];
    for t in &targets {
        let mut exports: Vec<_> = lib
            .exports_for(t)
            .iter()
            .map(|s| format!("{:?}{}", s.kind, s.name))
            .collect();
        exports.sort();
        out.push(format!("{t} exports {}", exports.join(" ")));
        // Undefineds are left out: llvm-readtapi's v5 writer drops them and
        // its v4 writer loses weak references, so they cannot round-trip.
        let mut reexports = lib.reexported_libraries_for(t);
        reexports.sort_unstable();
        out.push(format!("{t} reexports {}", reexports.join(" ")));
        let mut clients = lib.allowable_clients_for(t);
        clients.sort_unstable();
        out.push(format!("{t} clients {}", clients.join(" ")));
        out.push(format!("{t} umbrella {:?}", lib.parent_umbrella_for(t)));
    }
    out
}

fn stub_views(stub: &TextStub) -> BTreeMap<String, Vec<String>> {
    stub.libraries
        .iter()
        .map(|lib| (lib.install_name.clone(), stub_view(lib)))
        .collect()
}

#[test]
fn tbd_v4_libsystem() {
    let stub = stub("libSystem-v4.tbd");
    assert_eq!(stub.libraries.len(), 5);
    let main = stub.main().unwrap();
    assert_eq!(main.tbd_version, 4);
    assert_eq!(main.install_name, "/usr/lib/libSystem.B.dylib");
    assert_eq!(main.current_version, PackedVersion::new(1336, 0, 0));
    assert_eq!(main.targets.len(), 6);
    let arm64 = target("arm64-macos");
    let x86 = target("x86_64-macos");
    assert!(main.has_target(&target("arm64e-maccatalyst")));
    assert!(!main.has_target(&target("arm64-ios")));
    assert_eq!(
        main.reexported_libraries_for(&arm64),
        [
            "/usr/lib/system/libcache.dylib",
            "/usr/lib/system/libdyld.dylib",
            "/usr/lib/system/libsystem_c.dylib",
            "/usr/lib/system/libsystem_malloc.dylib"
        ]
    );
    assert_eq!(main.reexported_libraries_for(&x86).len(), 5);
    let exports = main.exports_for(&arm64);
    let names: Vec<_> = exports.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "__ZdlPv",
            "___crashreporter_info__",
            "_libSystem_atfork_child",
            "_libSystem_atfork_parent",
            "_libSystem_atfork_prepare"
        ]
    );
    assert_eq!(exports[0].kind, StubSymbolKind::Weak);
    assert!(
        main.exports_for(&x86)
            .iter()
            .any(|s| s.name == "R8289209$_close")
    );

    // Inlined libraries.
    let dyld = stub.library("/usr/lib/system/libdyld.dylib").unwrap();
    assert_eq!(dyld.current_version, PackedVersion::new(1122, 1, 0));
    assert_eq!(dyld.parent_umbrella_for(&arm64), Some("System"));
    assert_eq!(
        dyld.allowable_clients_for(&arm64),
        ["libSystem_debug", "System"]
    );
    let dyld_exports = dyld.exports_for(&arm64);
    assert!(dyld_exports.iter().any(|s| s.name == "dyld_stub_binder"));
    assert!(
        !dyld_exports
            .iter()
            .any(|s| s.name == "_dyld_stub_binder_x86")
    );
    assert!(
        dyld_exports
            .iter()
            .any(|s| s.name == "__tlv_bootstrap_marker" && s.kind == StubSymbolKind::ThreadLocal)
    );
    let libc = stub.library("/usr/lib/system/libsystem_c.dylib").unwrap();
    assert_eq!(libc.current_version, PackedVersion::new(1534, 40, 2));
    assert!(
        libc.exports_for(&arm64)
            .iter()
            .any(|s| s.name == "__setjmp_arm64")
    );
    assert!(
        !libc
            .exports_for(&x86)
            .iter()
            .any(|s| s.name == "__setjmp_arm64")
    );
}

#[test]
fn tbd_v4_objc_framework() {
    let stub = stub("Foundation-v4.tbd");
    let lib = stub.main().unwrap();
    assert_eq!(lib.swift_abi_version, 5);
    assert!(lib.flags.not_app_extension_safe && !lib.flags.flat_namespace);
    assert_eq!(lib.compatibility_version, PackedVersion::new(300, 0, 0));
    let macos = lib.exports_for(&target("arm64-macos"));
    let names: Vec<_> = macos
        .iter()
        .map(|s| (s.name.as_str(), s.reexported))
        .collect();
    assert_eq!(
        names,
        [
            ("_CFRelease", true),
            ("_CFRetain", true),
            ("_NSLog", false),
            ("_NSStringFromClass", false),
            ("_OBJC_CLASS_$_NSException", false),
            ("_OBJC_CLASS_$_NSObject", false),
            ("_OBJC_CLASS_$_NSString", false),
            ("_OBJC_EHTYPE_$_NSException", false),
            ("_OBJC_IVAR_$_NSString._length", false),
            ("_OBJC_METACLASS_$_NSException", false),
            ("_OBJC_METACLASS_$_NSObject", false),
            ("_OBJC_METACLASS_$_NSString", false),
        ]
    );
    let sim = lib.exports_for(&target("x86_64-ios-simulator"));
    assert_eq!(sim.len(), 3);
    assert!(lib.exports_for(&target("arm64-tvos")).is_empty());
}

#[test]
fn tbd_v3_multiple_documents() {
    let stub = stub("libobjc-v3.tbd");
    assert_eq!(stub.libraries.len(), 2);
    let lib = stub.main().unwrap();
    assert_eq!(lib.tbd_version, 3);
    assert!(lib.flags.flat_namespace);
    let names: Vec<_> = lib.targets.iter().map(ToString::to_string).collect();
    assert_eq!(names, ["i386-macos", "x86_64-macos", "x86_64h-macos"]);
    let x86 = target("x86_64-macos");
    assert_eq!(lib.parent_umbrella_for(&x86), Some("System"));
    assert_eq!(lib.allowable_clients_for(&x86), ["QldRuntimeClient"]);
    assert!(lib.allowable_clients_for(&target("i386-macos")).is_empty());
    assert_eq!(
        lib.reexported_libraries_for(&x86),
        ["/usr/lib/libqldobjc-trampolines.dylib"]
    );
    let exports = lib.exports_for(&x86);
    assert!(
        exports
            .iter()
            .any(|s| s.name == "_objc_weak_default" && s.kind == StubSymbolKind::Weak)
    );
    assert!(
        exports
            .iter()
            .any(|s| s.name == "_OBJC_IVAR_$_NSObject.isa")
    );
    assert_eq!(lib.exports_for(&target("i386-macos")).len(), 2);
    let trampolines = stub
        .library("/usr/lib/libqldobjc-trampolines.dylib")
        .unwrap();
    assert_eq!(trampolines.exports_for(&target("x86_64h-macos")).len(), 1);
}

#[test]
fn tbd_v5_matches_v4() {
    for (v4, v5) in [
        ("libSystem-v4.tbd", "libSystem-v5.tbd"),
        ("Foundation-v4.tbd", "Foundation-v5.tbd"),
    ] {
        let a = stub(v4);
        let b = stub(v5);
        assert_eq!(b.main().unwrap().tbd_version, 5);
        assert_eq!(
            a.main().unwrap().install_name,
            b.main().unwrap().install_name
        );
        assert_eq!(stub_views(&a), stub_views(&b), "{v4} vs {v5}");
        let undefined = |stub: &TextStub| {
            let lib = stub.main().unwrap();
            let mut out: Vec<_> = lib
                .targets
                .iter()
                .flat_map(|t| {
                    lib.undefineds
                        .iter()
                        .filter(|s| s.applies_to(t))
                        .flat_map(|s| {
                            s.symbols
                                .iter()
                                .map(|n| format!("{t} {n}"))
                                .chain(s.weak_symbols.iter().map(|n| format!("{t} weak {n}")))
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            out.sort();
            out
        };
        assert_eq!(undefined(&a), undefined(&b), "{v4} vs {v5}");
    }
    let foundation = stub("Foundation-v5.tbd");
    let lib = foundation.main().unwrap();
    assert_eq!(lib.undefineds[0].weak_symbols, ["_swift_task_alloc"]);
}

/// Converts the `.tbd` fixtures to other versions with `llvm-readtapi` and
/// checks that qld reads the same libraries from each.
#[test]
fn tbd_conversions_with_llvm_readtapi() {
    if !tool_works("llvm-readtapi") {
        println!("SKIPPED: llvm-readtapi is not installed");
        return;
    }
    for name in ["libSystem-v4.tbd", "Foundation-v4.tbd", "libobjc-v3.tbd"] {
        let original = stub(name);
        for filetype in ["tbd-v4", "tbd-v5"] {
            let out = scratch_dir().join(format!("{name}.{filetype}"));
            let output = Command::new("llvm-readtapi")
                .arg(data_dir().join(name))
                .arg(format!("--filetype={filetype}"))
                .arg("-o")
                .arg(&out)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let data = std::fs::read(&out).unwrap();
            let converted = TextStub::parse(&data, src(&out)).unwrap();
            assert_eq!(
                stub_views(&original),
                stub_views(&converted),
                "{name} as {filetype}"
            );
        }
    }
    // llvm-readtapi agrees that the committed v4 and v5 fixtures describe
    // the same libraries.
    for (a, b) in [
        ("libSystem-v4.tbd", "libSystem-v5.tbd"),
        ("Foundation-v4.tbd", "Foundation-v5.tbd"),
    ] {
        let output = Command::new("llvm-readtapi")
            .arg("-compare")
            .arg(data_dir().join(a))
            .arg(data_dir().join(b))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

/// Compares each library's per-architecture symbols with `llvm-nm`, which
/// merges the platforms of an architecture.
#[test]
fn tbd_symbols_match_llvm_nm() {
    for name in [
        "libSystem-v4.tbd",
        "libSystem-v5.tbd",
        "Foundation-v4.tbd",
        "Foundation-v5.tbd",
        "libobjc-v3.tbd",
    ] {
        let path = data_dir().join(name);
        let Some(text) = run_tool("llvm-nm", &[], &path) else {
            return;
        };
        let parsed = stub(name);
        let mut expected: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        let mut current = None;
        for line in text.lines() {
            if let Some(header) = line.strip_suffix("):")
                && let Some((install, arch)) = header.split_once(" (for architecture ")
            {
                current = Some((install.to_owned(), arch.to_owned()));
                expected.entry(current.clone().unwrap()).or_default();
                continue;
            }
            let fields: Vec<_> = line.split_whitespace().collect();
            let (kind, symbol) = match fields.as_slice() {
                [kind, symbol] => (*kind, *symbol),
                [_, kind, symbol] => (*kind, *symbol),
                _ => continue,
            };
            let kind = match kind {
                "U" => "undefined",
                "w" => "weak-undefined",
                "W" => "weak",
                _ => "defined",
            };
            expected
                .get_mut(current.as_ref().expect("llvm-nm header"))
                .unwrap()
                .push(format!("{kind} {symbol}"));
        }
        assert!(!expected.is_empty(), "{name}");
        for ((install, arch), mut symbols) in expected {
            symbols.sort();
            let lib = parsed.library(&install).expect("library listed by llvm-nm");
            let mut actual: Vec<String> = Vec::new();
            for t in lib.targets.iter().filter(|t| t.arch_name == arch) {
                for s in lib.exports_for(t) {
                    let kind = if s.kind == StubSymbolKind::Weak {
                        "weak"
                    } else {
                        "defined"
                    };
                    actual.push(format!("{kind} {}", s.name));
                }
                for section in lib.undefineds.iter().filter(|s| s.applies_to(t)) {
                    actual.extend(section.symbols.iter().map(|n| format!("undefined {n}")));
                    actual.extend(
                        section
                            .weak_symbols
                            .iter()
                            .map(|n| format!("weak-undefined {n}")),
                    );
                }
            }
            actual.sort();
            actual.dedup();
            assert_eq!(actual, symbols, "{name}: {install} {arch}");
        }
    }
}

/// Reads the `.tbd` files of an installed macOS SDK, when there is one.
#[test]
fn tbd_macos_sdk() {
    let Ok(output) = Command::new("xcrun").args(["--show-sdk-path"]).output() else {
        println!("SKIPPED: xcrun is not available");
        return;
    };
    if !output.status.success() {
        println!("SKIPPED: no macOS SDK");
        return;
    }
    let sdk = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let libsystem = sdk.join("usr/lib/libSystem.tbd");
    let Ok(data) = std::fs::read(&libsystem) else {
        println!("SKIPPED: {} not found", libsystem.display());
        return;
    };
    let stub = TextStub::parse(&data, src(&libsystem)).unwrap();
    let arm64 = target("arm64-macos");
    let main = stub.main().unwrap();
    assert!(main.install_name.starts_with("/usr/lib/libSystem"));
    assert!(main.has_target(&arm64));
    let exported = |symbol: &str| {
        stub.libraries
            .iter()
            .any(|lib| lib.exports_for(&arm64).iter().any(|s| s.name == symbol))
    };
    assert!(exported("_malloc"), "libSystem exports _malloc");
    assert!(exported("_dlopen"));

    let mut parsed = 0;
    for dir in ["usr/lib", "usr/lib/swift"] {
        let Ok(entries) = std::fs::read_dir(sdk.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "tbd") {
                let data = std::fs::read(&path).unwrap();
                TextStub::parse(&data, src(&path))
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                parsed += 1;
            }
        }
    }
    let frameworks = sdk.join("System/Library/Frameworks");
    for framework in ["Foundation", "CoreFoundation", "AppKit", "Security"] {
        let path = frameworks.join(format!("{framework}.framework/{framework}.tbd"));
        if let Ok(data) = std::fs::read(&path) {
            TextStub::parse(&data, src(&path))
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            parsed += 1;
        }
    }
    println!("parsed {parsed} SDK text stubs");
}

/// Parses every `.tbd` file under `$QLD_TBD_CORPUS` (for example an SDK or
/// the LLVM test inputs), reporting all failures at once. Files under a
/// directory named `invalid` are expected to fail or parse; they only must
/// not panic.
#[test]
fn tbd_corpus_from_env() {
    let Some(root) = std::env::var_os("QLD_TBD_CORPUS") else {
        println!("SKIPPED: QLD_TBD_CORPUS is not set");
        return;
    };
    let mut pending = vec![PathBuf::from(root)];
    let mut failures = Vec::new();
    let mut parsed = 0;
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "tbd") {
                let data = std::fs::read(&path).unwrap();
                if !matches!(
                    qld::input::identify(&data),
                    qld::input::FileFormat::Text(qld::input::identify::TextKind::Tbd)
                ) {
                    // Placeholder files named `.tbd` that are not stubs.
                    continue;
                }
                let result = TextStub::parse(&data, src(&path));
                let invalid = path
                    .to_string_lossy()
                    .rsplit(['/', '\\'])
                    .any(|c| c == "invalid" || c.contains("invalid-"));
                match result {
                    Ok(_) => parsed += 1,
                    Err(e) if !invalid => failures.push(e.to_string()),
                    Err(_) => {}
                }
            }
        }
    }
    println!("parsed {parsed} text stubs");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn tbd_crlf_line_endings() {
    for name in ["libSystem-v4.tbd", "libobjc-v3.tbd", "Foundation-v5.tbd"] {
        let data = fixture(name);
        let crlf = String::from_utf8(data)
            .unwrap()
            .replace("\r\n", "\n")
            .replace('\n', "\r\n");
        let path = data_dir().join(name);
        let converted = TextStub::parse(crlf.as_bytes(), src(&path)).unwrap();
        assert_eq!(stub_views(&stub(name)), stub_views(&converted), "{name}");
    }
}

#[test]
fn tbd_corruption_never_panics() {
    const NASTY: &[u8] = b"-:[]{},'\"\\\n #|>!&*~ \t\r0x";
    let mut rng = Rng(0x7bd0_7bd0_7bd0_7bd1);
    let path = Path::new("fuzz.tbd");
    let use_stub = |stub: &TextStub| {
        for lib in &stub.libraries {
            for t in &lib.targets {
                let _ = lib.exports_for(t);
                let _ = lib.reexported_libraries_for(t);
            }
        }
    };
    for name in [
        "libSystem-v4.tbd",
        "Foundation-v4.tbd",
        "libobjc-v3.tbd",
        "libSystem-v5.tbd",
        "Foundation-v5.tbd",
    ] {
        let data = fixture(name);
        let step = (data.len() / 300).max(1);
        for len in (0..data.len()).step_by(step) {
            if let Ok(stub) = TextStub::parse(&data[..len], src(path)) {
                use_stub(&stub);
            }
        }
        for _ in 0..1500 {
            let mut copy = data.clone();
            for _ in 0..1 + rng.below(6) {
                let at = rng.below(copy.len());
                match rng.below(4) {
                    0 => copy[at] = NASTY[rng.below(NASTY.len())],
                    1 => {
                        copy.remove(at);
                    }
                    2 => copy.insert(at, NASTY[rng.below(NASTY.len())]),
                    _ => copy[at] = rng.next() as u8,
                }
            }
            if let Ok(stub) = TextStub::parse(&copy, src(path)) {
                use_stub(&stub);
            }
        }
    }
    // Deep nesting in both syntaxes.
    let deep = [
        format!(
            "--- !tapi-tbd\ntbd-version: 4\ntargets: {}\n",
            "[".repeat(100_000)
        ),
        format!(
            "{{\"tapi_tbd_version\": 5, \"main_library\": {}}}",
            "[".repeat(100_000)
        ),
        format!("--- !tapi-tbd\n{}", "- ".repeat(50_000)),
        (0..5_000)
            .map(|i| format!("{}- \n", " ".repeat(i)))
            .collect(),
    ];
    for text in deep {
        assert!(TextStub::parse(text.as_bytes(), src(path)).is_err());
    }
}
