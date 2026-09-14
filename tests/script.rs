//! Integration tests for the linker script front end (workstream W3).
//!
//! Fixtures live in `tests/data/script/`: default scripts printed by
//! `ld --verbose` for several emulations (their notice permits copying),
//! glibc's `libc.so` and `libm.so` input scripts, and two scripts written for
//! these tests in the style of the Linux kernel and a Cortex-M SDK.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use qld::script::*;

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/script")
        .join(name)
}

fn parse_file(name: &str) -> Script {
    let path = data(name);
    let bytes = std::fs::read(&path).expect("fixture");
    parse_script(&bytes, &path, &mut NoIncludes).unwrap_or_else(|e| panic!("{e}"))
}

fn parse_str(src: &str) -> Result<Script, ScriptError> {
    parse_script(src.as_bytes(), Path::new("test.ld"), &mut NoIncludes)
}

fn sections(script: &Script) -> Vec<&SectionsCommand> {
    script
        .commands
        .iter()
        .filter_map(|c| match &c.kind {
            CommandKind::Sections(cmds) => Some(cmds),
            _ => None,
        })
        .flatten()
        .collect()
}

fn output_sections(script: &Script) -> Vec<&OutputSection> {
    sections(script)
        .into_iter()
        .filter_map(|c| match &c.kind {
            SectionsCommandKind::OutputSection(os) => Some(&**os),
            _ => None,
        })
        .collect()
}

fn find<'a>(script: &'a Script, name: &str) -> &'a OutputSection {
    output_sections(script)
        .into_iter()
        .find(|os| os.name == name.as_bytes())
        .unwrap_or_else(|| panic!("no output section {name}"))
}

fn inputs(os: &OutputSection) -> Vec<&InputSectionDescription> {
    os.commands
        .iter()
        .filter_map(|c| match &c.kind {
            OutputSectionCommandKind::Input(i) => Some(i),
            _ => None,
        })
        .collect()
}

/// Which output section a section of a plain object file lands in, first
/// match wins.
fn place<'a>(script: &'a Script, file: &str, section: &str) -> Option<&'a [u8]> {
    output_sections(script).into_iter().find_map(|os| {
        inputs(os)
            .iter()
            .any(|i| {
                i.matches(file.as_bytes(), None, section.as_bytes())
                    .is_some()
            })
            .then_some(os.name.as_slice())
    })
}

// ----- default scripts ------------------------------------------------------

#[test]
fn default_scripts_parse() {
    let dir = data("");
    let mut count = 0;
    for entry in std::fs::read_dir(dir).expect("data dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "x") {
            let bytes = std::fs::read(&path).expect("read");
            let script =
                parse_script(&bytes, &path, &mut NoIncludes).unwrap_or_else(|e| panic!("{e}"));
            assert!(!sections(&script).is_empty(), "{}", path.display());
            count += 1;
        }
    }
    assert!(
        count >= 8,
        "expected the ld --verbose fixtures, found {count}"
    );
}

#[test]
fn elf_x86_64_default_script_structure() {
    let script = parse_file("ld-verbose-elf_x86_64.x");
    let first = &script.commands[0];
    assert!(matches!(
        &first.kind,
        CommandKind::OutputFormat { default, big: Some(_), little: Some(_) } if default == b"elf64-x86-64"
    ));
    assert_eq!(first.span.line, 6);
    assert!(
        script
            .commands
            .iter()
            .any(|c| matches!(&c.kind, CommandKind::Entry(e) if e == b"_start"))
    );
    let search_dirs = script
        .commands
        .iter()
        .filter(|c| matches!(c.kind, CommandKind::SearchDir(_)))
        .count();
    assert_eq!(search_dirs, 10);

    let names: Vec<_> = output_sections(&script)
        .iter()
        .map(|os| String::from_utf8_lossy(&os.name).into_owned())
        .collect();
    for expected in [
        ".interp",
        ".text",
        ".tdata",
        ".bss",
        ".debug_info",
        "/DISCARD/",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} in {names:?}"
        );
    }

    let eh_frames: Vec<_> = output_sections(&script)
        .into_iter()
        .filter(|os| os.name == b".eh_frame")
        .collect();
    assert_eq!(eh_frames.len(), 2);
    assert_eq!(eh_frames[0].constraint, SectionConstraint::OnlyIfRo);
    assert_eq!(eh_frames[1].constraint, SectionConstraint::OnlyIfRw);
    assert!(inputs(eh_frames[0])[0].keep);

    let comment = find(&script, ".comment");
    assert_eq!(comment.section_type, OutputSectionType::Info);
    assert_eq!(comment.address, Some(Expr::Number(0)));
    assert!(
        comment
            .commands
            .iter()
            .any(|c| c.kind == OutputSectionCommandKind::LinkerVersion)
    );

    let init_array = find(&script, ".init_array");
    let descs = inputs(init_array);
    let sorted = &descs[0].sections.as_ref().unwrap()[0];
    assert_eq!(sorted.sort, SortMode::InitPriority);
    let excluded = &descs[1].sections.as_ref().unwrap()[1];
    assert_eq!(excluded.pattern.as_bytes(), b".ctors");
    assert_eq!(excluded.exclude_files.len(), 4);

    let init = find(&script, ".init");
    let spec = &inputs(init)[0].sections.as_ref().unwrap()[0];
    assert_eq!(spec.sort, SortMode::NoSort);

    let data = find(&script, ".data");
    assert!(
        data.commands
            .iter()
            .any(|c| c.kind == OutputSectionCommandKind::Constructors { sorted: true })
    );

    let lrodata = find(&script, ".lrodata");
    assert!(lrodata.address.is_some());

    // DATA_SEGMENT_RELRO_END (SIZEOF (.got.plt) >= 24 ? 24 : 0, .)
    let relro = sections(&script).into_iter().find_map(|c| match &c.kind {
        SectionsCommandKind::Assignment(a) => match &a.expr {
            Expr::DataSegmentRelroEnd(offset, value) => Some((offset, value)),
            _ => None,
        },
        _ => None,
    });
    let (offset, value) = relro.expect("DATA_SEGMENT_RELRO_END");
    assert!(matches!(**offset, Expr::Conditional(..)));
    assert_eq!(**value, Expr::Dot);

    let rela_plt = find(&script, ".rela.plt");
    assert!(rela_plt.commands.iter().any(|c| matches!(
        &c.kind,
        OutputSectionCommandKind::Assignment(Assignment { kind: AssignKind::ProvideHidden, target, .. })
            if target == b"__rela_iplt_start"
    )));
}

#[test]
fn default_script_places_sections_like_gnu() {
    let script = parse_file("ld-verbose-elf_x86_64.x");
    let cases = [
        (".text", ".text"),
        (".text.hot.foo", ".text"),
        (".text.unlikely", ".text"),
        (".rodata.str1.1", ".rodata"),
        (".data.rel.ro.local", ".data.rel.ro"),
        (".tbss.x", ".tbss"),
        (".init_array.00100", ".init_array"),
        (".note.GNU-stack", ".note.GNU-stack"),
        (".gnu.lto_main.0", "/DISCARD/"),
        (".bss.x", ".bss"),
        ("COMMON", ".bss"),
        (".debug_line.foo", ".debug_line"),
    ];
    for (section, expected) in cases {
        assert_eq!(
            place(&script, "/tmp/foo.o", section),
            Some(expected.as_bytes()),
            "{section}"
        );
    }
    assert_eq!(place(&script, "/tmp/foo.o", ".unknown"), None);
    // EXCLUDE_FILE keeps crtbegin.o's `.ctors` out of `.init_array`, so it
    // lands in `.ctors`; other files' `.ctors` go to `.init_array`.
    assert_eq!(
        place(&script, "/usr/lib/gcc/crtbegin.o", ".ctors"),
        Some(&b".ctors"[..])
    );
    assert_eq!(
        place(&script, "/tmp/foo.o", ".ctors"),
        Some(&b".init_array"[..])
    );
}

#[test]
fn live_ld_verbose_parses() {
    let emulations = ["elf_x86_64", "elf_i386", "elf32_x86_64", "elf_iamcu"];
    for emulation in emulations {
        let Ok(output) = std::process::Command::new("ld")
            .args(["-m", emulation, "--verbose"])
            .output()
        else {
            eprintln!("ld not available; skipping");
            return;
        };
        if !output.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let Some(script) = text
            .split("==================================================")
            .nth(1)
        else {
            continue;
        };
        parse_str(script).unwrap_or_else(|e| panic!("{emulation}: {e}"));
    }
}

// ----- input scripts --------------------------------------------------------

#[test]
fn glibc_libc_so() {
    let script = parse_file("glibc-libc.so");
    assert_eq!(script.commands.len(), 2);
    assert!(matches!(
        &script.commands[0].kind,
        CommandKind::OutputFormat { default, big: None, little: None } if default == b"elf64-x86-64"
    ));
    let CommandKind::Group(files) = &script.commands[1].kind else {
        panic!("expected GROUP");
    };
    assert_eq!(
        files,
        &[
            InputFile {
                name: InputName::Path(b"/lib64/libc.so.6".to_vec()),
                as_needed: false
            },
            InputFile {
                name: InputName::Path(b"/usr/lib64/libc_nonshared.a".to_vec()),
                as_needed: false
            },
            InputFile {
                name: InputName::Path(b"/lib64/ld-linux-x86-64.so.2".to_vec()),
                as_needed: true
            },
        ]
    );
    let libm = parse_file("glibc-libm.so");
    assert!(
        matches!(&libm.commands[1].kind, CommandKind::Group(f) if f.len() == 2 && f[1].as_needed)
    );
}

#[test]
fn host_input_scripts_parse() {
    for path in [
        "/usr/lib64/libc.so",
        "/usr/lib/libc.so",
        "/usr/lib/x86_64-linux-gnu/libc.so",
    ] {
        if let Ok(bytes) = std::fs::read(path)
            && bytes.starts_with(b"/*")
        {
            parse_script(&bytes, Path::new(path), &mut NoIncludes)
                .unwrap_or_else(|e| panic!("{e}"));
        }
    }
}

#[test]
fn input_lists() {
    let script = parse_str(
        "INPUT(a.o b.o) GROUP(-lc =/usr/lib/crt1.o AS_NEEDED(-lgcc_s AS_NEEDED(x.so)) \"sp ace.o\")\n\
         INPUT(a.o, b.o) LIB(z.o)",
    )
    .unwrap();
    let CommandKind::Group(group) = &script.commands[1].kind else {
        panic!()
    };
    assert_eq!(group[0].name, InputName::Library(b"c".to_vec()));
    assert_eq!(group[1].name, InputName::Path(b"=/usr/lib/crt1.o".to_vec()));
    assert!(group[2].as_needed && group[3].as_needed);
    assert_eq!(group[3].name, InputName::Path(b"x.so".to_vec()));
    assert_eq!(group[4].name, InputName::Path(b"sp ace.o".to_vec()));
    assert!(!group[4].as_needed);
    // GNU ld glues the comma onto the file name.
    let CommandKind::Input(input) = &script.commands[2].kind else {
        panic!()
    };
    assert_eq!(input[0].name, InputName::Path(b"a.o,".to_vec()));
    assert!(matches!(script.commands[3].kind, CommandKind::Lib(_)));
}

#[test]
fn top_level_commands() {
    let script = parse_str(
        r#"
        OUTPUT(a.out) STARTUP(crt0.o) TARGET(elf64-x86-64) OUTPUT_ARCH(i386:x86-64)
        SEARCH_DIR(/opt/lib) MAP(out.map) LD_FEATURE("SANE_EXPR")
        EXTERN(foo bar, baz) FORCE_COMMON_ALLOCATION INHIBIT_COMMON_ALLOCATION
        FORCE_GROUP_ALLOCATION NOCROSSREFS(.text .data) NOCROSSREFS_TO(.a .b)
        MEMORY { ram : ORIGIN = 0, LENGTH = 1M }
        REGION_ALIAS("alias", ram)
        HLL() SYSLIB(c) FLOAT NOFLOAT
        SECTIONS { .foo : { *(.foo) } }
        INSERT AFTER .text
        a = 1; b += 2, c -= 3; d *= 4; e /= 5; f <<= 6; g >>= 7; h &= 8; i |= 9; j ^= 10;
        HIDDEN(k = 1); PROVIDE(l = 2); PROVIDE_HIDDEN(m = 3);
        ASSERT(1, "message")
        "#,
    )
    .unwrap();
    let kinds: Vec<_> = script.commands.iter().map(|c| &c.kind).collect();
    assert!(matches!(kinds[0], CommandKind::Output(o) if o == b"a.out"));
    assert!(matches!(kinds[1], CommandKind::Startup(_)));
    assert!(matches!(kinds[2], CommandKind::Target(_)));
    assert!(matches!(kinds[3], CommandKind::OutputArch(a) if a == b"i386:x86-64"));
    assert!(matches!(kinds[4], CommandKind::SearchDir(_)));
    assert!(matches!(kinds[5], CommandKind::Map(_)));
    assert!(matches!(kinds[6], CommandKind::LdFeature(f) if f == b"SANE_EXPR"));
    assert!(matches!(kinds[7], CommandKind::Extern(e) if e.len() == 3));
    assert!(matches!(kinds[8], CommandKind::ForceCommonAllocation));
    assert!(matches!(kinds[9], CommandKind::InhibitCommonAllocation));
    assert!(matches!(kinds[10], CommandKind::ForceGroupAllocation));
    assert!(matches!(kinds[11], CommandKind::NoCrossRefs(s) if s.len() == 2));
    assert!(matches!(kinds[12], CommandKind::NoCrossRefsTo(s) if s.len() == 2));
    assert!(matches!(kinds[13], CommandKind::Memory(_)));
    assert!(
        matches!(kinds[14], CommandKind::RegionAlias { alias, region } if alias == b"alias" && region == b"ram")
    );
    assert!(matches!(kinds[15], CommandKind::Sections(_)));
    assert!(matches!(
        kinds[16],
        CommandKind::Insert { position: InsertPosition::After, section } if section == b".text"
    ));
    let ops: Vec<_> = kinds[17..27]
        .iter()
        .map(|k| match k {
            CommandKind::Assignment(a) => a.op,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        ops,
        [
            AssignOp::Assign,
            AssignOp::Add,
            AssignOp::Sub,
            AssignOp::Mul,
            AssignOp::Div,
            AssignOp::Shl,
            AssignOp::Shr,
            AssignOp::And,
            AssignOp::Or,
            AssignOp::Xor
        ]
    );
    let assign_kinds: Vec<_> = kinds[27..30]
        .iter()
        .map(|k| match k {
            CommandKind::Assignment(a) => a.kind,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        assign_kinds,
        [
            AssignKind::Hidden,
            AssignKind::Provide,
            AssignKind::ProvideHidden
        ]
    );
    assert!(matches!(kinds[30], CommandKind::Assert(a) if a.message == b"message"));
    assert_eq!(kinds.len(), 31);
}

// ----- SECTIONS details -----------------------------------------------------

#[test]
fn output_section_attributes() {
    let script = parse_str(
        r#"SECTIONS {
          .a 0x1000 (NOLOAD) : AT(0x2000) ALIGN(16) ALIGN_WITH_INPUT SUBALIGN(4) ONLY_IF_RW {
            *(.a)
          } >ram AT>rom :text :data =0x90909090,
          .b (READONLY (TYPE = SHT_PROGBITS)) : { } =0x112233445566778899
          .c (TYPE = 7) : { FILL(0xab) } =(0x90)
          .d () : { } =1+1
          .e BIND(0x100) BLOCK(4) : { }
          .f (DSECT) : { } .g (COPY) : { } .h (OVERLAY) : { } .i (READONLY) : { }
          "quoted name" : { }
          .j ADDR(.a) + SIZEOF(.a) : { }
          .k (0x4000) : { }
        }"#,
    )
    .unwrap();
    let a = find(&script, ".a");
    assert_eq!(a.address, Some(Expr::Number(0x1000)));
    assert_eq!(a.section_type, OutputSectionType::NoLoad);
    assert_eq!(a.load_address, Some(Expr::Number(0x2000)));
    assert_eq!(a.align, Some(Expr::Number(16)));
    assert!(a.align_with_input);
    assert_eq!(a.subalign, Some(Expr::Number(4)));
    assert_eq!(a.constraint, SectionConstraint::OnlyIfRw);
    assert_eq!(a.region.as_deref(), Some(&b"ram"[..]));
    assert_eq!(a.load_region.as_deref(), Some(&b"rom"[..]));
    assert_eq!(a.phdrs, [b"text".to_vec(), b"data".to_vec()]);
    assert_eq!(
        a.fill.as_ref().unwrap().hex_digits.as_deref(),
        Some(&b"90909090"[..])
    );

    let b = find(&script, ".b");
    assert!(
        matches!(&b.section_type, OutputSectionType::ReadOnlyType(Expr::Symbol(s)) if s == b"SHT_PROGBITS")
    );
    assert_eq!(
        b.fill.as_ref().unwrap().hex_digits.as_deref(),
        Some(&b"112233445566778899"[..])
    );
    let c = find(&script, ".c");
    assert_eq!(c.section_type, OutputSectionType::Type(Expr::Number(7)));
    assert!(c.fill.as_ref().unwrap().hex_digits.is_some());
    assert!(
        find(&script, ".d")
            .fill
            .as_ref()
            .unwrap()
            .hex_digits
            .is_none()
    );
    assert_eq!(find(&script, ".d").section_type, OutputSectionType::Normal);
    assert_eq!(find(&script, ".e").address, Some(Expr::Number(0x100)));
    assert_eq!(find(&script, ".f").section_type, OutputSectionType::DSect);
    assert_eq!(find(&script, ".g").section_type, OutputSectionType::Copy);
    assert_eq!(find(&script, ".h").section_type, OutputSectionType::Overlay);
    assert_eq!(
        find(&script, ".i").section_type,
        OutputSectionType::ReadOnly
    );
    find(&script, "quoted name");
    assert!(matches!(
        find(&script, ".j").address,
        Some(Expr::Binary(BinaryOp::Add, ..))
    ));
    assert_eq!(find(&script, ".k").address, Some(Expr::Number(0x4000)));
}

#[test]
fn input_section_descriptions() {
    let script = parse_str(
        r#"SECTIONS { .out : {
            KEEP(*(.init))
            *(SORT_BY_NAME(SORT_BY_ALIGNMENT(.a.*)) SORT_BY_ALIGNMENT(SORT_BY_NAME(.b.*)))
            *(SORT_BY_ALIGNMENT(.c) REVERSE(.d) SORT(REVERSE(.e)) REVERSE(SORT_BY_INIT_PRIORITY(.f)))
            SORT(*)(.g)
            EXCLUDE_FILE(*crtend.o) *(.h)
            *(EXCLUDE_FILE(*a.o *b.o) .i .j)
            libc.a:printf.o(.text)
            foo.o
            INPUT_SECTION_FLAGS(SHF_ALLOC & !SHF_WRITE) *(.k)
            [ .l .m ]
            CREATE_OBJECT_SYMBOLS
            CONSTRUCTORS
            BYTE(1) SHORT(2) LONG(3) QUAD(4) SQUAD(-5)
            ASCIZ "hello"
            ASSERT(. > 0, "empty");
            . = ALIGN(8);
            ;
        } }"#,
    )
    .unwrap();
    let out = find(&script, ".out");
    let descs = inputs(out);
    assert!(descs[0].keep);
    let s = descs[1].sections.as_ref().unwrap();
    assert_eq!(s[0].sort, SortMode::NameAlignment);
    assert_eq!(s[1].sort, SortMode::AlignmentName);
    let s = descs[2].sections.as_ref().unwrap();
    assert_eq!((s[0].sort, s[0].reverse), (SortMode::Alignment, false));
    assert_eq!((s[1].sort, s[1].reverse), (SortMode::Name, true));
    assert_eq!((s[2].sort, s[2].reverse), (SortMode::Name, true));
    assert_eq!((s[3].sort, s[3].reverse), (SortMode::InitPriority, true));
    assert_eq!(descs[3].file.sort, SortMode::Name);
    assert_eq!(descs[4].file.exclude.len(), 1);
    assert!(descs[4].matches(b"x.o", None, b".h").is_some());
    assert!(descs[4].matches(b"/usr/crtend.o", None, b".h").is_none());
    let s = descs[5].sections.as_ref().unwrap();
    assert_eq!(s[0].exclude_files.len(), 2);
    assert!(s[1].exclude_files.is_empty());
    assert_eq!(descs[5].matches(b"a.o", None, b".i"), None);
    assert_eq!(descs[5].matches(b"a.o", None, b".j"), Some(1));
    assert_eq!(descs[5].matches(b"c.o", None, b".i"), Some(0));
    assert!(
        descs[6]
            .matches(b"printf.o", Some(b"libc.a"), b".text")
            .is_some()
    );
    assert!(descs[6].matches(b"printf.o", None, b".text").is_none());
    assert!(descs[7].sections.is_none());
    assert_eq!(descs[7].matches(b"foo.o", None, b".anything"), Some(0));
    assert_eq!(
        descs[8].flags,
        [
            SectionFlag {
                name: b"SHF_ALLOC".to_vec(),
                negated: false
            },
            SectionFlag {
                name: b"SHF_WRITE".to_vec(),
                negated: true
            }
        ]
    );
    assert_eq!(descs[9].sections.as_ref().unwrap().len(), 2);
    let kinds: Vec<_> = out.commands.iter().map(|c| &c.kind).collect();
    assert!(kinds.contains(&&OutputSectionCommandKind::CreateObjectSymbols));
    assert!(kinds.contains(&&OutputSectionCommandKind::Constructors { sorted: false }));
    let sizes: Vec<_> = kinds
        .iter()
        .filter_map(|k| match k {
            OutputSectionCommandKind::Data { size, .. } => Some(size.bytes()),
            _ => None,
        })
        .collect();
    assert_eq!(sizes, [1, 2, 4, 8, 8]);
    assert!(kinds.contains(&&OutputSectionCommandKind::Asciz(b"hello".to_vec())));
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, OutputSectionCommandKind::Assert(_)))
    );
    assert!(matches!(kinds.last(), Some(OutputSectionCommandKind::Assignment(a)) if a.is_dot()));
}

#[test]
fn overlay_and_phdrs_and_memory() {
    let script = parse_str(
        r#"
        MEMORY {
          rom (rx) : ORIGIN = 0, LENGTH = 64K
          ram (!rx) : o = 0x20000000, l = 4M,
          io (a!wl) : org = 0x40000000 len = 1K
        }
        PHDRS {
          headers PT_PHDR PHDRS ;
          interp PT_INTERP ;
          text PT_LOAD FILEHDR PHDRS AT (0x100) FLAGS (5) ;
          stack PT_GNU_STACK ;
          custom 0x6fffffff ;
        }
        SECTIONS {
          OVERLAY 0x1000 : NOCROSSREFS AT (0x4000) SUBALIGN(2) {
            .ov1 { *(.ov1) } :text =0x11
            .ov2 { *(.ov2) }
          } >ram AT>rom :text =0x22,
          ENTRY(start)
        }
        VERSION {
          VERS_1.0 { global: foo; bar*; local: *; };
          VERS_2.0 { global: extern "C++" { "ns::f()"; ns::g*; }; } VERS_1.0;
        }
        "#,
    )
    .unwrap();
    let CommandKind::Memory(regions) = &script.commands[0].kind else {
        panic!()
    };
    assert_eq!(regions.len(), 3);
    assert_eq!(
        regions[0].attributes.flags,
        MemoryAttributes::READ_ONLY | MemoryAttributes::EXEC
    );
    assert_eq!(
        regions[1].attributes.not_flags,
        MemoryAttributes::READ_ONLY | MemoryAttributes::EXEC
    );
    assert_eq!(regions[1].origin, Expr::Number(0x2000_0000));
    assert_eq!(regions[1].length, Expr::Number(4 << 20));
    assert_eq!(regions[2].attributes.flags, MemoryAttributes::ALLOC);
    assert_eq!(
        regions[2].attributes.not_flags,
        MemoryAttributes::WRITE | MemoryAttributes::LOAD
    );
    assert_eq!(regions[2].span.line, 5);

    let CommandKind::Phdrs(phdrs) = &script.commands[1].kind else {
        panic!()
    };
    assert_eq!(phdrs.len(), 5);
    assert_eq!(phdrs[0].phdr_type, Expr::Number(6));
    assert!(phdrs[0].phdrs && !phdrs[0].filehdr);
    assert_eq!(phdrs[2].phdr_type, Expr::Number(1));
    assert!(phdrs[2].filehdr && phdrs[2].phdrs);
    assert_eq!(phdrs[2].at, Some(Expr::Number(0x100)));
    assert_eq!(phdrs[2].flags, Some(Expr::Number(5)));
    assert_eq!(phdrs[3].phdr_type, Expr::Number(0x6474_e551));
    assert_eq!(phdrs[4].phdr_type, Expr::Number(0x6fff_ffff));

    let cmds = sections(&script);
    let SectionsCommandKind::Overlay(overlay) = &cmds[0].kind else {
        panic!()
    };
    assert_eq!(overlay.address, Some(Expr::Number(0x1000)));
    assert!(overlay.no_cross_refs);
    assert_eq!(overlay.load_address, Some(Expr::Number(0x4000)));
    assert_eq!(overlay.sections.len(), 2);
    assert_eq!(overlay.sections[0].phdrs, [b"text".to_vec()]);
    assert!(overlay.sections[0].fill.is_some());
    assert_eq!(overlay.region.as_deref(), Some(&b"ram"[..]));
    assert_eq!(overlay.load_region.as_deref(), Some(&b"rom"[..]));
    assert!(matches!(&cmds[1].kind, SectionsCommandKind::Entry(e) if e == b"start"));

    let CommandKind::Version(nodes) = &script.commands[3].kind else {
        panic!()
    };
    assert_eq!(nodes[0].name.as_deref(), Some(&b"VERS_1.0"[..]));
    assert_eq!(nodes[0].globals.len(), 2);
    assert_eq!(nodes[0].locals[0].pattern, b"*");
    assert_eq!(nodes[1].dependencies, [b"VERS_1.0".to_vec()]);
    assert_eq!(nodes[1].globals.len(), 2);
    assert!(nodes[1].globals[0].literal);
    assert_eq!(nodes[1].globals[1].language.as_deref(), Some(&b"C++"[..]));
    assert_eq!(nodes[1].globals[1].pattern, b"ns::g*");
}

#[test]
fn phdr_errors() {
    let e = parse_str("PHDRS { text PT_BOGUS; }").unwrap_err();
    assert!(e.message.contains("unknown phdr type"), "{e}");
    let e = parse_str("PHDRS { text PT_LOAD FLAGS; }").unwrap_err();
    assert!(e.message.contains("PHDRS syntax error"), "{e}");
}

#[test]
fn version_scripts() {
    let nodes = parse_version_script(
        b"# comment\n{ global: a; b; local: *; };\nV1 { c; };\nV2 { d; } V1;\n",
        Path::new("vers"),
    )
    .unwrap();
    assert_eq!(nodes.len(), 3);
    assert!(nodes[0].name.is_none());
    assert_eq!(nodes[2].dependencies, [b"V1".to_vec()]);
    assert!(parse_version_script(b"V1 { a; }", Path::new("v")).is_err());
}

#[test]
fn defsym_and_expression_entry_points() {
    let a = parse_defsym(b"foo=0x1000+bar").unwrap();
    assert_eq!(a.target, b"foo");
    assert!(matches!(a.expr, Expr::Binary(BinaryOp::Add, ..)));
    assert!(parse_defsym(b"a-b=1").is_err());
    assert!(parse_defsym(b"foo=1 2").is_err());
    assert_eq!(
        parse_expression(b"(1)", Path::new("e")).unwrap(),
        Expr::Number(1)
    );
}

#[test]
fn gnu_tokenization_quirks() {
    // `foo=1` is one name in the script state, so this is a syntax error in
    // GNU ld too.
    assert!(parse_str("foo=1;").is_err());
    assert!(parse_str("foo = 1;").is_ok());
    // In an expression `/` is part of a symbol name.
    let script = parse_str("x = foo/2;").unwrap();
    assert!(
        matches!(&script.commands[0].kind, CommandKind::Assignment(a) if a.expr == Expr::Symbol(b"foo/2".to_vec()))
    );
    // `-` is not.
    let script = parse_str("x = foo-2;").unwrap();
    assert!(
        matches!(&script.commands[0].kind, CommandKind::Assignment(a) if matches!(a.expr, Expr::Binary(BinaryOp::Sub, ..)))
    );
    // Suffixed numbers.
    let script = parse_str("x = each + 1fh + 17o + 101b + 99d + 4K + 2M + $10;").unwrap();
    let CommandKind::Assignment(a) = &script.commands[0].kind else {
        panic!()
    };
    let mut ctx = Layout::default();
    assert_eq!(
        eval_absolute(&a.expr, &mut ctx).unwrap(),
        0xeac + 0x1f + 15 + 5 + 99 + 4096 + (2 << 20) + 16
    );
    // Inside an output section, `foo=1;` is an input file name.
    let script = parse_str("SECTIONS { .x : { foo=1; } }").unwrap();
    assert_eq!(
        inputs(find(&script, ".x"))[0].file.pattern.as_bytes(),
        b"foo=1"
    );
    // `/DISCARD/` after a fill expression is not a division.
    let script = parse_str("SECTIONS { .a : { } =0x90 /DISCARD/ : { *(.x) } }").unwrap();
    assert!(find(&script, "/DISCARD/").is_discard());
    // The separator after an assignment is required.
    assert!(parse_str("SECTIONS { .a : { . = 1 } }").is_err());
    assert!(parse_str("a = 1").is_err());
}

#[test]
fn errors_carry_positions() {
    let e = parse_str("SECTIONS {\n  .text : {\n    *(.text\n  }\n}").unwrap_err();
    assert_eq!(e.file, Path::new("test.ld"));
    assert_eq!((e.line, e.column), (4, 3), "{e}");
    assert!(
        e.to_string().starts_with("test.ld:4:3: syntax error"),
        "{e}"
    );
    let error: qld::Error = e.into();
    assert!(error.to_string().contains("line 4"), "{error}");

    let e = parse_str("x = 1;\ny = @;").unwrap_err();
    assert_eq!((e.line, e.column), (2, 5));
    let e = parse_str("/* unterminated").unwrap_err();
    assert!(e.message.contains("unterminated comment"));
    let e = parse_str("SECTIONS {").unwrap_err();
    assert!(e.message.contains("end of file"), "{e}");
}

// ----- INCLUDE --------------------------------------------------------------

#[derive(Default)]
struct MemoryReader {
    files: HashMap<Vec<u8>, &'static str>,
    requests: Vec<(Vec<u8>, PathBuf)>,
}

impl ScriptReader for MemoryReader {
    fn read_include(&mut self, name: &[u8], from: &Path) -> io::Result<(PathBuf, Vec<u8>)> {
        self.requests.push((name.to_vec(), from.to_path_buf()));
        match self.files.get(name) {
            Some(text) => Ok((
                PathBuf::from(format!("/inc/{}", String::from_utf8_lossy(name))),
                text.as_bytes().to_vec(),
            )),
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }
}

#[test]
fn include_everywhere() {
    let mut reader = MemoryReader::default();
    reader
        .files
        .insert(b"top.ld".to_vec(), "ENTRY(main)\nINCLUDE nested.ld");
    reader.files.insert(b"nested.ld".to_vec(), "x = 1;");
    reader
        .files
        .insert(b"regions.ld".to_vec(), "ram : ORIGIN = 0, LENGTH = 1K");
    reader
        .files
        .insert(b"secs.ld".to_vec(), ".inc : { *(.inc) }\ny = 2;");
    reader.files.insert(b"stmts.ld".to_vec(), "*(.s1)\nBYTE(1)");
    let src = "INCLUDE top.ld\nMEMORY { INCLUDE regions.ld rom : ORIGIN = 0, LENGTH = 1K }\n\
               SECTIONS { INCLUDE secs.ld .out : { INCLUDE stmts.ld *(.s2) } }";
    let script = parse_script(src.as_bytes(), Path::new("main.ld"), &mut reader).unwrap();
    assert!(matches!(&script.commands[0].kind, CommandKind::Entry(e) if e == b"main"));
    assert!(matches!(&script.commands[1].kind, CommandKind::Assignment(a) if a.target == b"x"));
    assert_eq!(
        script.file_of(script.commands[1].span),
        Path::new("/inc/nested.ld")
    );
    assert_eq!(
        script.file_of(script.commands[0].span),
        Path::new("/inc/top.ld")
    );
    assert!(matches!(&script.commands[2].kind, CommandKind::Memory(r) if r.len() == 2));
    find(&script, ".inc");
    let out = find(&script, ".out");
    assert_eq!(out.commands.len(), 3);
    assert_eq!(
        reader.requests[1],
        (b"nested.ld".to_vec(), PathBuf::from("/inc/top.ld"))
    );

    // Errors inside an included file point into it.
    let mut reader = MemoryReader::default();
    reader.files.insert(b"bad.ld".to_vec(), "\n\n  oops");
    let e = parse_script(b"INCLUDE bad.ld", Path::new("main.ld"), &mut reader).unwrap_err();
    assert_eq!(e.file, Path::new("/inc/bad.ld"));
    assert_eq!(e.line, 3);
    let e = parse_script(b"INCLUDE missing.ld", Path::new("main.ld"), &mut reader).unwrap_err();
    assert!(e.message.contains("cannot open"), "{e}");
    assert!(parse_script(b"INCLUDE x.ld", Path::new("m"), &mut NoIncludes).is_err());

    // Recursive includes stop at the depth limit instead of looping.
    let mut reader = MemoryReader::default();
    reader.files.insert(b"self.ld".to_vec(), "INCLUDE self.ld");
    let e = parse_script(b"INCLUDE self.ld", Path::new("m"), &mut reader).unwrap_err();
    assert!(e.message.contains("nested too deeply"), "{e}");
}

// ----- evaluation -----------------------------------------------------------

#[derive(Clone, Copy)]
struct Section {
    vma: u64,
    lma: u64,
    size: u64,
    align: u64,
}

/// A hand-made layout: the evaluation context used by these tests.
#[derive(Default)]
struct Layout {
    sections: Vec<(&'static str, Section)>,
    symbols: HashMap<Vec<u8>, Value<usize>>,
    regions: HashMap<&'static str, (u64, u64)>,
    dot: Option<u64>,
    current: Option<usize>,
    segments: HashMap<&'static str, u64>,
    ignore_asserts: bool,
}

impl Layout {
    fn section_index(&self, name: &[u8]) -> Result<usize, EvalError> {
        self.sections
            .iter()
            .position(|(n, _)| n.as_bytes() == name)
            .ok_or_else(|| EvalError::UndefinedSection(name.to_vec()))
    }

    fn section(&self, name: &[u8]) -> Result<Section, EvalError> {
        let index = if name == b"NEXT_SECTION" {
            self.current.map_or(0, |c| c + 1)
        } else {
            self.section_index(name)?
        };
        self.sections
            .get(index)
            .map(|(_, s)| *s)
            .ok_or_else(|| EvalError::UndefinedSection(name.to_vec()))
    }
}

impl EvalContext for Layout {
    type Section = usize;

    fn section_vma(&self, section: usize) -> u64 {
        self.sections[section].1.vma
    }
    fn current_section(&self) -> Option<usize> {
        self.current
    }
    fn dot(&self) -> Result<u64, EvalError> {
        self.dot.ok_or(EvalError::NoLocationCounter)
    }
    fn symbol(&mut self, name: &[u8]) -> Result<Value<usize>, EvalError> {
        self.symbols
            .get(name)
            .copied()
            .ok_or_else(|| EvalError::UndefinedSymbol(name.to_vec()))
    }
    fn is_defined(&mut self, name: &[u8]) -> bool {
        self.symbols.contains_key(name)
    }
    fn section_addr(&mut self, name: &[u8]) -> Result<Value<usize>, EvalError> {
        Ok(Value::relative(self.section_index(name)?, 0))
    }
    fn section_load_addr(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        Ok(self.section(name)?.lma)
    }
    fn section_size(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        Ok(self.section(name)?.size)
    }
    fn section_alignment(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        Ok(self.section(name)?.align)
    }
    fn region_origin(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        self.regions
            .iter()
            .find(|(n, _)| n.as_bytes() == name)
            .map(|(_, r)| r.0)
            .ok_or_else(|| EvalError::UndefinedRegion(name.to_vec()))
    }
    fn region_length(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        self.regions
            .iter()
            .find(|(n, _)| n.as_bytes() == name)
            .map(|(_, r)| r.1)
            .ok_or_else(|| EvalError::UndefinedRegion(name.to_vec()))
    }
    fn sizeof_headers(&mut self) -> Result<u64, EvalError> {
        Ok(0x1c8)
    }
    fn max_page_size(&self) -> Result<u64, EvalError> {
        Ok(0x1000)
    }
    fn common_page_size(&self) -> Result<u64, EvalError> {
        Ok(0x1000)
    }
    fn segment_start(&mut self, name: &[u8], default: u64) -> u64 {
        self.segments
            .iter()
            .find(|(n, _)| n.as_bytes() == name)
            .map_or(default, |(_, v)| *v)
    }
    fn assertion_failed(&mut self, message: &[u8]) -> Result<(), EvalError> {
        if self.ignore_asserts {
            Ok(())
        } else {
            Err(EvalError::AssertionFailed(message.to_vec()))
        }
    }
}

fn sample_layout() -> Layout {
    let mut layout = Layout {
        sections: vec![
            (
                ".text",
                Section {
                    vma: 0x401000,
                    lma: 0x401000,
                    size: 0x1234,
                    align: 16,
                },
            ),
            (
                ".data",
                Section {
                    vma: 0x403000,
                    lma: 0x8000_0000,
                    size: 0x100,
                    align: 8,
                },
            ),
        ],
        dot: Some(0x402234),
        ..Layout::default()
    };
    layout.regions.insert("ram", (0x2000_0000, 0x2_0000));
    layout
        .symbols
        .insert(b"main".to_vec(), Value::relative(0, 0x10));
    layout
        .symbols
        .insert(b"abs".to_vec(), Value::absolute(0x42));
    layout.segments.insert("text-segment", 0x10000);
    layout
}

fn eval_str(src: &str, layout: &mut Layout) -> Result<u64, EvalError> {
    let expr = parse_expression(src.as_bytes(), Path::new("expr")).expect("parse");
    eval_absolute(&expr, layout)
}

#[test]
fn every_builtin() {
    let mut l = sample_layout();
    let cases: &[(&str, u64)] = &[
        ("ABSOLUTE(main)", 0x401010),
        ("ADDR(.data)", 0x403000),
        ("ALIGN(0x1000)", 0x403000),
        ("ALIGN(0x1001, 0x100)", 0x1100),
        ("ALIGNOF(.text)", 16),
        ("ALIGNOF(NEXT_SECTION)", 16),
        ("BLOCK(0x10)", 0x402240),
        ("DATA_SEGMENT_ALIGN(0x1000, 0x1000)", 0x403234),
        ("DATA_SEGMENT_END(0x5000)", 0x5000),
        ("DATA_SEGMENT_RELRO_END(24, 0x6000)", 0x6000),
        ("DEFINED(main)", 1),
        ("DEFINED(nosuch)", 0),
        ("LENGTH(ram)", 0x2_0000),
        ("LOADADDR(.data)", 0x8000_0000),
        ("LOG2CEIL(0x1001)", 13),
        ("MAX(3, 7)", 7),
        ("MIN(3, 7)", 3),
        ("NEXT(0x100)", 0x402300),
        ("ORIGIN(ram)", 0x2000_0000),
        ("SEGMENT_START(\"text-segment\", 0x400000)", 0x10000),
        ("SEGMENT_START(\"data-segment\", 0x400000)", 0x400000),
        ("SIZEOF(.text)", 0x1234),
        ("SIZEOF_HEADERS", 0x1c8),
        ("CONSTANT(MAXPAGESIZE)", 0x1000),
        ("CONSTANT (COMMONPAGESIZE)", 0x1000),
        ("ASSERT(1, \"never\") + 1", 2),
        ("abs + 1", 0x43),
        (". - ADDR(.text)", 0x1234),
        ("SIZEOF(.got.plt) >= 24 ? 24 : 0", 0),
    ];
    // SIZEOF(.got.plt) is undefined here: the last case must fail.
    for (src, expected) in &cases[..cases.len() - 1] {
        assert_eq!(eval_str(src, &mut l), Ok(*expected), "{src}");
    }
    assert!(matches!(
        eval_str("SIZEOF(.got.plt) >= 24 ? 24 : 0", &mut l),
        Err(EvalError::UndefinedSection(_))
    ));
    assert!(matches!(
        eval_str("CONSTANT(FOO)", &mut l),
        Err(EvalError::UnknownConstant(_))
    ));
    assert!(matches!(
        eval_str("ORIGIN(rom)", &mut l),
        Err(EvalError::UndefinedRegion(_))
    ));
    assert!(matches!(
        eval_str("nosuch", &mut l),
        Err(EvalError::UndefinedSymbol(_))
    ));
    assert_eq!(
        eval_str("ASSERT(0, \"boom\")", &mut l),
        Err(EvalError::AssertionFailed(b"boom".to_vec()))
    );
    l.ignore_asserts = true;
    assert_eq!(eval_str("ASSERT(0, \"boom\")", &mut l), Ok(0));
    assert_eq!(eval_str("10 / 0", &mut l), Err(EvalError::DivisionByZero));
    l.dot = None;
    assert_eq!(
        eval_str("ALIGN(8)", &mut l),
        Err(EvalError::NoLocationCounter)
    );
}

#[test]
fn default_script_expressions_evaluate() {
    let script = parse_file("ld-verbose-elf_x86_64.x");
    let mut layout = sample_layout();
    layout.dot = Some(0x400000);
    // `. = SEGMENT_START("text-segment", 0x400000) + SIZEOF_HEADERS;`
    let first_dot = sections(&script)
        .into_iter()
        .find_map(|c| match &c.kind {
            SectionsCommandKind::Assignment(a) if a.is_dot() => Some(a),
            _ => None,
        })
        .unwrap();
    layout.segments.clear();
    assert_eq!(eval_dot_assignment(first_dot, &mut layout), Ok(0x4001c8));
    layout.segments.insert("text-segment", 0x10000);
    assert_eq!(eval_dot_assignment(first_dot, &mut layout), Ok(0x101c8));
    // `PROVIDE (__executable_start = SEGMENT_START(...))` is absolute but
    // flagged as coming from `.`-like values.
    let provide = sections(&script)
        .into_iter()
        .find_map(|c| match &c.kind {
            SectionsCommandKind::Assignment(a) if a.kind == AssignKind::Provide => Some(a),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        eval_symbol_assignment(provide, &mut layout),
        Ok(Value::absolute(0x10000).with_from_dot(true))
    );
}

#[test]
fn cortex_m_script() {
    let script = parse_file("cortex-m.ld");
    let CommandKind::Memory(regions) = &script
        .commands
        .iter()
        .find(|c| matches!(c.kind, CommandKind::Memory(_)))
        .unwrap()
        .kind
    else {
        unreachable!()
    };
    // Evaluate MEMORY as GNU ld does, with no location counter.
    let mut layout = Layout::default();
    let evaluated: Vec<_> = regions
        .iter()
        .map(|r| {
            (
                String::from_utf8_lossy(&r.name).into_owned(),
                eval_absolute(&r.origin, &mut layout).unwrap(),
                eval_absolute(&r.length, &mut layout).unwrap(),
            )
        })
        .collect();
    assert_eq!(
        evaluated,
        [
            ("FLASH".to_string(), 0x0800_0000, 1024 << 10),
            ("RAM".to_string(), 0x2000_0000, 128 << 10),
            ("CCMRAM".to_string(), 0x1000_0000, 64 << 10),
        ]
    );
    assert_eq!(regions[2].attributes.not_flags, MemoryAttributes::EXEC);
    for (name, origin, length) in &evaluated {
        let name: &'static str = Box::leak(name.clone().into_boxed_str());
        layout.regions.insert(name, (*origin, *length));
    }

    // `_estack = ORIGIN(RAM) + LENGTH(RAM);` — GNU ld: 0x20020000, absolute.
    let estack = script
        .commands
        .iter()
        .find_map(|c| match &c.kind {
            CommandKind::Assignment(a) if a.target == b"_estack" => Some(a),
            _ => None,
        })
        .unwrap();
    let value = eval_symbol_assignment(estack, &mut layout).unwrap();
    assert_eq!(value.section, ValueSection::Absolute);
    assert_eq!(value.value, 0x2002_0000);

    let data = find(&script, ".data");
    assert_eq!(data.region.as_deref(), Some(&b"RAM"[..]));
    assert_eq!(data.load_region.as_deref(), Some(&b"FLASH"[..]));
    assert_eq!(
        find(&script, ".text").region.as_deref(),
        Some(&b"REGION_TEXT"[..])
    );

    // `.fill_test` holds 1 + 2 + 4 + 8 bytes of data and `. += 4`: GNU ld
    // makes it 0x13 bytes.
    let fill_test = find(&script, ".fill_test");
    let mut size = 0;
    layout.sections.push((
        ".fill_test",
        Section {
            vma: 0x0800_0078,
            lma: 0x0800_0078,
            size: 0,
            align: 1,
        },
    ));
    layout.current = Some(0);
    for command in &fill_test.commands {
        layout.dot = Some(0x0800_0078 + size);
        match &command.kind {
            OutputSectionCommandKind::Data { size: s, .. } => size += s.bytes(),
            OutputSectionCommandKind::Assignment(a) if a.is_dot() => {
                size = eval_dot_assignment(a, &mut layout).unwrap() - 0x0800_0078;
            }
            OutputSectionCommandKind::Fill(fill) => {
                assert_eq!(fill_pattern(fill, &mut layout).unwrap(), [0x90, 0x90]);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(size, 0x13);
    assert_eq!(
        fill_pattern(fill_test.fill.as_ref().unwrap(), &mut layout).unwrap(),
        [0xff]
    );

    // /DISCARD/ archive patterns.
    let discard = find(&script, "/DISCARD/");
    let descs = inputs(discard);
    assert!(
        descs[0]
            .matches(b"printf.o", Some(b"/usr/lib/libc.a"), b".text")
            .is_some()
    );
    assert!(descs[0].matches(b"printf.o", None, b".text").is_none());
    assert!(
        descs[1]
            .matches(b"sin.o", Some(b"libm.a"), b".text")
            .is_some()
    );
    assert!(
        descs[2]
            .matches(b"x.o", Some(b"/lib/libgcc.a"), b".text")
            .is_some()
    );
    assert!(
        descs[2]
            .matches(b"crt0.o", Some(b"/lib/libgcc.a"), b".text")
            .is_none()
    );
}

#[test]
fn kernel_style_script() {
    let script = parse_file("kernel-style.ld");
    let CommandKind::Phdrs(phdrs) = &script
        .commands
        .iter()
        .find(|c| matches!(c.kind, CommandKind::Phdrs(_)))
        .unwrap()
        .kind
    else {
        unreachable!()
    };
    assert_eq!(phdrs.len(), 5);
    assert_eq!(phdrs[4].phdr_type, Expr::Number(4));
    let text = find(&script, ".text");
    assert_eq!(text.phdrs, [b"text".to_vec()]);
    assert_eq!(
        text.fill.as_ref().unwrap().hex_digits.as_deref(),
        Some(&b"cccccccc"[..])
    );
    assert!(text.load_address.is_some());
    assert_eq!(find(&script, ".notes").phdrs.len(), 2);
    assert_eq!(
        find(&script, ".data..percpu").address,
        Some(Expr::Number(0))
    );
    let rodata = find(&script, ".rodata");
    let ksymtab = &inputs(rodata)[2];
    assert!(ksymtab.keep);
    assert!(
        ksymtab
            .matches(b"a.o", None, b"___ksymtab+printk")
            .is_some()
    );
    // `. = ASSERT(...)` at the top level, and a bare ASSERT.
    let last = &script.commands[script.commands.len() - 1];
    assert!(matches!(last.kind, CommandKind::Assert(_)));
    let assert_assignment = &script.commands[script.commands.len() - 2];
    assert!(
        matches!(&assert_assignment.kind, CommandKind::Assignment(a) if a.is_dot() && matches!(a.expr, Expr::Assert(..)))
    );

    // The symbols the script reads, for resolution to treat as referenced.
    let mut referenced = Vec::new();
    if let CommandKind::Assignment(a) = &assert_assignment.kind {
        a.expr.for_each_symbol(&mut |name| {
            referenced.push(String::from_utf8_lossy(name).into_owned())
        });
    }
    assert_eq!(referenced, ["_end", "_text"]);
}

// ----- robustness -----------------------------------------------------------

/// Deterministic xorshift generator, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Parses `bytes`; when that succeeds, evaluates every expression found with
/// a permissive context. Must never panic.
fn exercise(bytes: &[u8]) {
    let mut reader = MemoryReader::default();
    reader.files.insert(b"inc.ld".to_vec(), "x = 1;");
    let Ok(script) = parse_script(bytes, Path::new("fuzz.ld"), &mut reader) else {
        return;
    };
    let mut layout = sample_layout();
    layout.ignore_asserts = true;
    let mut visit = |expr: &Expr| {
        let _ = eval(expr, &mut layout);
    };
    for command in &script.commands {
        match &command.kind {
            CommandKind::Assignment(a) => visit(&a.expr),
            CommandKind::Sections(cmds) => {
                for c in cmds {
                    if let SectionsCommandKind::OutputSection(os) = &c.kind {
                        for oc in &os.commands {
                            match &oc.kind {
                                OutputSectionCommandKind::Assignment(a) => visit(&a.expr),
                                OutputSectionCommandKind::Input(i) => {
                                    let _ = i.matches(b"a.o", Some(b"lib.a"), b".text.x");
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[test]
fn corrupted_scripts_never_panic() {
    let corpus: Vec<Vec<u8>> = [
        "ld-verbose-elf_x86_64.x",
        "i386pep.x",
        "kernel-style.ld",
        "cortex-m.ld",
        "glibc-libc.so",
    ]
    .iter()
    .map(|name| std::fs::read(data(name)).unwrap())
    .collect();
    let extra = b"MEMORY { r (rwx) : ORIGIN = 0, LENGTH = 1K } PHDRS { t PT_LOAD FILEHDR; } \
        SECTIONS { OVERLAY : { .a { *(SORT(.a)) } } INCLUDE inc.ld } VERSION { V { global: *; }; }"
        .to_vec();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let specials = b"(){}[];:,=*?\"/\\!<>&|^~-+.#\n\0\xff";
    for base in corpus.iter().chain([&extra]) {
        // Truncation at many points.
        let step = (base.len() / 400).max(1);
        for cut in (0..base.len()).step_by(step) {
            exercise(&base[..cut]);
        }
        // Random byte replacement, insertion and deletion.
        for _ in 0..300 {
            let mut bytes = base.clone();
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(bytes.len().max(1));
                match rng.below(3) {
                    0 if !bytes.is_empty() => bytes[at] = specials[rng.below(specials.len())],
                    1 => bytes.insert(at.min(bytes.len()), specials[rng.below(specials.len())]),
                    _ if !bytes.is_empty() => {
                        bytes.remove(at);
                    }
                    _ => {}
                }
            }
            exercise(&bytes);
        }
    }
    // Random token soup.
    let words: &[&str] = &[
        "SECTIONS",
        "MEMORY",
        "PHDRS",
        "{",
        "}",
        "(",
        ")",
        ";",
        ":",
        ",",
        "=",
        ".",
        "*",
        "KEEP",
        "SORT",
        "ALIGN",
        "0x10",
        "foo",
        ".text",
        ">",
        "AT",
        "?",
        "+",
        "-",
        "/DISCARD/",
        "\"s\"",
        "ORIGIN",
        "LENGTH",
        "INPUT",
        "AS_NEEDED",
        "OVERLAY",
        "EXCLUDE_FILE",
        "PROVIDE",
        "FILL",
        "ASSERT",
        "VERSION",
        "global",
        "extern",
        "INCLUDE",
        "inc.ld",
        "DEFINED",
        "SEGMENT_START",
    ];
    for _ in 0..3000 {
        let len = rng.below(40);
        let text: Vec<&str> = (0..len).map(|_| words[rng.below(words.len())]).collect();
        exercise(text.join(" ").as_bytes());
    }
}

#[test]
fn hostile_nesting_is_rejected_without_overflow() {
    // Run on a small stack to prove recursion is bounded.
    std::thread::Builder::new()
        .stack_size(1 << 21)
        .spawn(|| {
            let n = 100_000;
            let parens = format!("x = {}1{};", "(".repeat(n), ")".repeat(n));
            assert!(parse_str(&parens).is_err());
            let unary = format!("x = {}1;", "-".repeat(n));
            assert!(parse_str(&unary).is_err());
            let chain = format!("x = 1{};", "+1".repeat(n));
            assert!(parse_str(&chain).is_err());
            let ternary = format!("x = {}1{};", "1?".repeat(n), ":1".repeat(n));
            assert!(parse_str(&ternary).is_err());
            let needed = format!("INPUT({}a{})", "AS_NEEDED(".repeat(n), ")".repeat(n));
            assert!(parse_str(&needed).is_err());
            let sorts = format!(
                "SECTIONS {{ .a : {{ *({}.a{}) }} }}",
                "SORT(".repeat(n),
                ")".repeat(n)
            );
            assert!(parse_str(&sorts).is_err());
            // Moderate nesting still works and evaluates.
            let ok = format!("x = {}1{};", "(".repeat(100), " + 1)".repeat(100));
            let script = parse_str(&ok).unwrap();
            let CommandKind::Assignment(a) = &script.commands[0].kind else {
                panic!()
            };
            assert_eq!(eval_absolute(&a.expr, &mut Layout::default()), Ok(101));
        })
        .unwrap()
        .join()
        .unwrap();
}

/// Parses every script under the directories in `QLD_SCRIPT_SWEEP`
/// (colon-separated), for example binutils' `ldscripts` directories. Files
/// that GNU ld itself rejects will be reported too, so review the output.
#[test]
#[ignore = "needs QLD_SCRIPT_SWEEP"]
fn sweep_script_directories() {
    let Ok(dirs) = std::env::var("QLD_SCRIPT_SWEEP") else {
        return;
    };
    let mut stack: Vec<PathBuf> = dirs.split(':').map(PathBuf::from).collect();
    let (mut ok, mut failed) = (0, 0);
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let mut reader = FsReader {
            search_dirs: path.parent().map(Path::to_path_buf).into_iter().collect(),
        };
        match parse_script(&bytes, &path, &mut reader) {
            Ok(_) => ok += 1,
            // Version scripts and dynamic lists have their own syntax.
            Err(_) if parse_version_script(&bytes, &path).is_ok() => ok += 1,
            Err(e) => {
                failed += 1;
                println!("{e}");
            }
        }
    }
    println!("parsed {ok}, failed {failed}");
}
