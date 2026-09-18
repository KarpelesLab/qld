//! arm64e: pointer authentication (`ARM64_RELOC_AUTHENTICATED_POINTER`),
//! the arm64e chained fixup formats (`DYLD_CHAINED_PTR_ARM64E` up to macOS
//! 11, `_USERLAND24` from macOS 12, as ld64 chooses), `__auth_stubs` and
//! `__auth_got`, the `cpusubtype`, and arm64e targets of `.tbd` files.
//!
//! Stock macOS does not run third-party arm64e code, and `ld64.lld`
//! rejects authenticated pointers, so the outputs are checked structurally:
//! the fixup chains are walked here, independently of qld's writer, using
//! the layouts of dyld's `<mach-o/fixup-chains.h>`, and every fixup is
//! checked against the symbol it must reach.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use qld::macho::read::consts::LC_DYLD_CHAINED_FIXUPS;
use qld::macho::read::{MachOFile, Source};

use super::{
    clang_for, link_bytes, os, scratch, skip, symbols, syslibroot, tool, tool_works, u32_le,
};

/// The imports table: (name, addend) by index.
fn imports(data: &[u8]) -> Vec<(String, i64)> {
    let source = Source::new(Path::new("out"));
    let file = MachOFile::parse(data, source).unwrap();
    let command = file
        .find_command(LC_DYLD_CHAINED_FIXUPS)
        .unwrap()
        .expect("LC_DYLD_CHAINED_FIXUPS");
    let blob = command.linkedit_data().unwrap();
    let bytes = &data[blob.dataoff as usize..][..blob.datasize as usize];
    let fixups = qld::macho::read::ChainedFixups::parse(
        bytes,
        u64::from(blob.dataoff),
        file.endian(),
        source,
    )
    .unwrap();
    fixups
        .imports()
        .map(|i| {
            let i = i.unwrap();
            (String::from_utf8_lossy(i.name).into_owned(), i.addend)
        })
        .collect()
}

/// One decoded chained fixup.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pointer {
    /// A rebase to an unslid address.
    Rebase { target: u64, auth: Option<Auth> },
    /// A bind to an import, with its addend (inline or from the table).
    Bind {
        name: String,
        addend: i64,
        auth: Option<Auth>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Auth {
    key: u8,
    address_diversity: bool,
    diversity: u16,
}

/// The segments of an image: (name, vmaddr, vmsize, fileoff).
fn segments(data: &[u8]) -> Vec<(String, u64, u64, u64)> {
    let file = MachOFile::parse(data, Source::new(Path::new("out"))).unwrap();
    file.load_commands()
        .filter_map(|c| c.unwrap().segment().ok())
        .map(|s| {
            (
                String::from_utf8_lossy(s.name).into_owned(),
                s.vmaddr,
                s.vmsize,
                s.fileoff,
            )
        })
        .collect()
}

fn u16_le(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(data[at..at + 2].try_into().unwrap())
}

fn u64_le(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
}

/// Walks the fixup chains of `data`: the pointer formats used and every
/// fixup by address.
fn walk_chains(data: &[u8]) -> (Vec<u16>, BTreeMap<u64, Pointer>) {
    let file = MachOFile::parse(data, Source::new(Path::new("out"))).unwrap();
    let command = file
        .find_command(LC_DYLD_CHAINED_FIXUPS)
        .unwrap()
        .expect("LC_DYLD_CHAINED_FIXUPS");
    let blob = command.linkedit_data().unwrap();
    let blob = &data[blob.dataoff as usize..][..blob.datasize as usize];
    let imports = imports(data);
    let starts_offset = u32_le(blob, 4) as usize;
    let segments = segments(data);
    let base = segments
        .iter()
        .find(|s| s.0 == "__TEXT")
        .map(|s| s.1)
        .unwrap();
    let seg_count = u32_le(blob, starts_offset) as usize;
    let mut formats = Vec::new();
    let mut out = BTreeMap::new();
    for segment in 0..seg_count {
        let offset = u32_le(blob, starts_offset + 4 + segment * 4) as usize;
        if offset == 0 {
            continue;
        }
        let starts = starts_offset + offset;
        let page_size = u64::from(u16_le(blob, starts + 4));
        let format = u16_le(blob, starts + 6);
        let segment_offset = u64_le(blob, starts + 8);
        let page_count = usize::from(u16_le(blob, starts + 20));
        formats.push(format);
        let (_, vmaddr, _, fileoff) = segments
            .iter()
            .find(|s| s.1 == base + segment_offset)
            .cloned()
            .expect("segment of a chain");
        let stride = if format == 2 || format == 6 { 4 } else { 8 };
        for page in 0..page_count {
            let start = u16_le(blob, starts + 22 + page * 2);
            if start == 0xffff {
                continue;
            }
            let mut at = page as u64 * page_size + u64::from(start);
            loop {
                let raw = u64_le(data, (fileoff + at) as usize);
                let address = vmaddr + at;
                let (pointer, next) = decode(format, raw, base, &imports);
                assert!(out.insert(address, pointer).is_none(), "{address:#x} twice");
                if next == 0 {
                    break;
                }
                at += next * stride;
            }
        }
    }
    formats.sort_unstable();
    formats.dedup();
    (formats, out)
}

/// Decodes one chained pointer of `format`; returns it and `next`.
fn decode(format: u16, raw: u64, base: u64, imports: &[(String, i64)]) -> (Pointer, u64) {
    let bits = |shift: u32, width: u32| (raw >> shift) & ((1u64 << width) - 1);
    let import = |ordinal: u64, inline: i64| {
        let (name, addend) = &imports[ordinal as usize];
        (name.clone(), addend + inline)
    };
    match format {
        // DYLD_CHAINED_PTR_64
        2 => {
            let next = bits(51, 12);
            if bits(63, 1) == 1 {
                let (name, addend) = import(bits(0, 24), bits(24, 8) as i64);
                (
                    Pointer::Bind {
                        name,
                        addend,
                        auth: None,
                    },
                    next,
                )
            } else {
                let target = bits(0, 36) | (bits(36, 8) << 56);
                (Pointer::Rebase { target, auth: None }, next)
            }
        }
        // DYLD_CHAINED_PTR_ARM64E, _USERLAND24
        1 | 12 => {
            let next = bits(51, 11);
            let bind = bits(62, 1) == 1;
            let authenticated = bits(63, 1) == 1;
            let auth = authenticated.then(|| Auth {
                key: bits(49, 2) as u8,
                address_diversity: bits(48, 1) == 1,
                diversity: bits(32, 16) as u16,
            });
            let ordinal_bits = if format == 1 { 16 } else { 24 };
            let pointer = match (bind, authenticated) {
                (false, false) => {
                    let target = bits(0, 43) | (bits(43, 8) << 56);
                    // ARM64E: unslid address; USERLAND24: image offset.
                    let target = if format == 1 { target } else { base + target };
                    Pointer::Rebase { target, auth: None }
                }
                (false, true) => Pointer::Rebase {
                    target: base + bits(0, 32),
                    auth,
                },
                (true, false) => {
                    // 19-bit signed addend.
                    let addend = ((bits(32, 19) << 45) as i64) >> 45;
                    let (name, addend) = import(bits(0, ordinal_bits), addend);
                    Pointer::Bind {
                        name,
                        addend,
                        auth: None,
                    }
                }
                (true, true) => {
                    let (name, addend) = import(bits(0, ordinal_bits), 0);
                    Pointer::Bind { name, addend, auth }
                }
            };
            (pointer, next)
        }
        other => panic!("unexpected pointer format {other}"),
    }
}

fn compile_arm64e(dir: &Path, source: &str, target: &str) -> std::path::PathBuf {
    let input = super::data_dir().join(source);
    let stem = Path::new(source).file_stem().unwrap().to_str().unwrap();
    let object = dir.join(format!("{stem}-{target}.o"));
    let compiler = if source.ends_with(".cpp") {
        "clang++"
    } else {
        "clang"
    };
    let status = Command::new(compiler)
        .arg(format!("--target={target}"))
        // The stub libc++ has no sized `operator delete`.
        .args(["-O1", "-fno-sized-deallocation", "-c"])
        .arg(&input)
        .arg("-o")
        .arg(&object)
        .status()
        .unwrap();
    assert!(status.success(), "compiling {source} for {target}");
    object
}

fn arm64e_args(object: &Path, min: &str, output: &Path) -> Vec<std::ffi::OsString> {
    let root = syslibroot();
    os(&[
        "-arch",
        "arm64e",
        "-platform_version",
        "macos",
        min,
        min,
        "-syslibroot",
        root.to_str().unwrap(),
        object.to_str().unwrap(),
        "-lc++",
        "-lSystem",
        "-o",
        output.to_str().unwrap(),
    ])
}

/// The address of a symbol, defined or not in the external symbol table
/// sense (local ones included).
fn address(data: &[u8], name: &str) -> u64 {
    symbols(data)
        .into_iter()
        .find(|s| s.is_defined() && s.name == name)
        .unwrap_or_else(|| panic!("no symbol {name}"))
        .n_value
}

/// (segment, section, address, size) of each section.
fn sections(data: &[u8]) -> Vec<(String, String, u64, u64)> {
    let file = MachOFile::parse(data, Source::new(Path::new("out"))).unwrap();
    let mut out = Vec::new();
    for command in file.load_commands() {
        if let Ok(segment) = command.unwrap().segment() {
            for section in segment.sections.iter() {
                out.push((
                    String::from_utf8_lossy(section.segname).into_owned(),
                    String::from_utf8_lossy(section.sectname).into_owned(),
                    section.addr,
                    section.size,
                ));
            }
        }
    }
    out
}

fn section(data: &[u8], name: &str) -> (u64, u64) {
    sections(data)
        .into_iter()
        .find(|s| s.1 == name)
        .map(|s| (s.2, s.3))
        .unwrap_or_else(|| panic!("no section {name}"))
}

#[test]
fn arm64e_pointer_authentication() {
    if !clang_for("arm64e") {
        skip(
            "arm64e_pointer_authentication",
            "clang cannot target arm64e-apple-macos",
        );
        return;
    }
    let dir = scratch("arm64e");
    for (version, format) in [("13.0", 12u16), ("11.0", 1)] {
        let target = format!("arm64e-apple-macos{version}");
        let compiled = compile_arm64e(&dir, "ptrauth.cpp", &target);
        // The object, and the same through `-r`, which keeps the
        // authenticated pointers and their relocations.
        let relocatable = dir.join(format!("ptrauth-{version}-r.o"));
        let (merged, _) = link_bytes(&os(&[
            "-arch",
            "arm64e",
            "-r",
            compiled.to_str().unwrap(),
            "-o",
            relocatable.to_str().unwrap(),
        ]))
        .unwrap_or_else(|e| panic!("{version}: -r: {e}"));
        std::fs::write(&relocatable, &merged).unwrap();
        assert_eq!(u32_le(&merged, 8), 0x8000_0002, "-r cpusubtype");
        for (input, object) in [("object", &compiled), ("-r", &relocatable)] {
            // For messages.
            let min = format!("{version} {input}");
            let output = dir.join(format!("ptrauth-{version}-{input}"));
            let (bytes, _) = link_bytes(&arm64e_args(object, version, &output))
                .unwrap_or_else(|e| panic!("{min}: {e}"));
            std::fs::write(&output, &bytes).unwrap();

            // The header: arm64e with the versioned pointer authentication ABI.
            assert_eq!(u32_le(&bytes, 4), 0x0100_000c, "cputype");
            assert_eq!(u32_le(&bytes, 8), 0x8000_0002, "cpusubtype");

            let (formats, fixups) = walk_chains(&bytes);
            assert_eq!(formats, [format], "{min}");
            let at = |name: &str| {
                fixups
                    .get(&address(&bytes, name))
                    .unwrap_or_else(|| panic!("{min}: no fixup at {name}: {fixups:#x?}"))
                    .clone()
            };
            let ia = |address_diversity, diversity| {
                Some(Auth {
                    key: 0,
                    address_diversity,
                    diversity,
                })
            };
            // A signed pointer to a local function.
            assert_eq!(
                at("_local_pointer"),
                Pointer::Rebase {
                    target: address(&bytes, "__ZL14local_functionv"),
                    auth: ia(false, 0),
                },
                "{min}"
            );
            // A signed pointer to an import.
            assert_eq!(
                at("_import_pointer"),
                Pointer::Bind {
                    name: "_puts".into(),
                    addend: 0,
                    auth: ia(false, 0),
                },
                "{min}"
            );
            // A plain data pointer.
            assert_eq!(
                at("_data_pointer"),
                Pointer::Rebase {
                    target: address(&bytes, "_data"),
                    auth: None,
                },
                "{min}"
            );
            // The vtable: offset-to-top, type info (plain), then the virtual
            // functions, signed with address diversity and a discriminator.
            let vtable = address(&bytes, "__ZTV4Base");
            assert_eq!(
                fixups[&(vtable + 8)],
                Pointer::Rebase {
                    target: address(&bytes, "__ZTI4Base"),
                    auth: None,
                }
            );
            for (slot, function) in [
                (16, "__ZN4BaseD1Ev"),
                (24, "__ZN4BaseD0Ev"),
                (32, "__ZN4Base5valueEv"),
            ] {
                match &fixups[&(vtable + slot)] {
                    Pointer::Rebase {
                        target,
                        auth: Some(auth),
                    } => {
                        assert_eq!(*target, address(&bytes, function), "{function}");
                        assert_eq!(auth.key, 0, "{function}");
                        assert!(auth.address_diversity, "{function}");
                        assert_ne!(auth.diversity, 0, "{function}");
                    }
                    other => panic!("{min}: {function}: {other:?}"),
                }
            }
            // The type info's vtable pointer: a DA-signed import with an
            // addend, which only the imports table can carry.
            assert_eq!(
                at("__ZTI4Base"),
                Pointer::Bind {
                    name: "__ZTVN10__cxxabiv117__class_type_infoE".into(),
                    addend: 16,
                    auth: Some(Auth {
                        key: 2,
                        address_diversity: false,
                        diversity: 0,
                    }),
                },
                "{min}"
            );
            // The thread-local variable's descriptor binds the plain thunk.
            let tlv = address(&bytes, "_tls");
            assert_eq!(
                fixups[&tlv],
                Pointer::Bind {
                    name: "__tlv_bootstrap".into(),
                    addend: 0,
                    auth: None,
                }
            );

            // Calls to imports go through __auth_stubs, which load their
            // __auth_got slot and branch with braa, the slot's address being
            // the discriminator; the slots are IA-signed, address-diversified
            // binds.
            let names: Vec<String> = sections(&bytes).into_iter().map(|s| s.1).collect();
            assert!(!names.iter().any(|n| n == "__stubs"), "{names:?}");
            let (stubs, stubs_size) = section(&bytes, "__auth_stubs");
            let (got, got_size) = section(&bytes, "__auth_got");
            assert_eq!(stubs_size % 16, 0);
            assert_eq!(stubs_size / 16, got_size / 8);
            let text = segments(&bytes)
                .into_iter()
                .find(|s| s.0 == "__TEXT")
                .unwrap();
            let mut stub_targets = Vec::new();
            for index in 0..stubs_size / 16 {
                let address = stubs + index * 16;
                let at = (text.3 + (address - text.1)) as usize;
                let adrp = u32_le(&bytes, at);
                let add = u32_le(&bytes, at + 4);
                assert_eq!(adrp & 0x9f00_001f, 0x9000_0011, "adrp x17");
                assert_eq!(add & 0xffc0_03ff, 0x9100_0231, "add x17, x17");
                assert_eq!(u32_le(&bytes, at + 8), 0xf940_0230, "ldr x16, [x17]");
                assert_eq!(u32_le(&bytes, at + 12), 0xd71f_0a11, "braa x16, x17");
                let immlo = u64::from((adrp >> 29) & 3);
                let immhi = u64::from((adrp >> 5) & 0x7ffff);
                let pages = (((immhi << 2) | immlo) << 43) as i64 >> 43;
                let slot = ((address & !0xfff) as i64 + (pages << 12)) as u64
                    + u64::from((add >> 10) & 0xfff);
                assert!(slot >= got && slot < got + got_size, "{slot:#x}");
                match &fixups[&slot] {
                    Pointer::Bind { name, addend, auth } => {
                        assert_eq!(*addend, 0);
                        assert_eq!(*auth, ia(true, 0), "{name}");
                        stub_targets.push(name.clone());
                    }
                    other => panic!("{min}: __auth_got slot {slot:#x}: {other:?}"),
                }
            }
            for called in ["_printf", "__Znwm", "__ZdlPv"] {
                assert!(
                    stub_targets.iter().any(|n| n == called),
                    "{min}: {called} in {stub_targets:?}"
                );
            }
        }
    }

    // arm64e images need chained fixups.
    let object = dir.join("ptrauth-arm64e-apple-macos13.0.o");
    let mut args = arm64e_args(&object, "13.0", &dir.join("never"));
    args.push("-no_fixup_chains".into());
    let error = link_bytes(&args).unwrap_err();
    assert!(error.contains("chained fixups"), "{error}");
}

/// A `.tbd` whose arm64 and arm64e targets export different symbols: each
/// link uses its own target's.
#[test]
fn arm64e_text_stub_targets() {
    if !clang_for("arm64e") || !clang_for("arm64") {
        skip(
            "arm64e_text_stub_targets",
            "clang cannot target arm64e-apple-macos",
        );
        return;
    }
    let dir = scratch("arm64e_tbd");
    std::fs::write(
        dir.join("libslices.tbd"),
        "--- !tapi-tbd\ntbd-version: 4\n\
         targets: [ arm64-macos, arm64e-macos ]\n\
         install-name: '/usr/lib/libslices.dylib'\n\
         exports:\n\
         \x20 - targets: [ arm64-macos ]\n\
         \x20   symbols: [ _for_arm64 ]\n\
         \x20 - targets: [ arm64e-macos ]\n\
         \x20   symbols: [ _for_arm64e ]\n...\n",
    )
    .unwrap();
    for (arch, wanted, other) in [
        ("arm64", "for_arm64", "for_arm64e"),
        ("arm64e", "for_arm64e", "for_arm64"),
    ] {
        for (symbol, works) in [(wanted, true), (other, false)] {
            let source = dir.join(format!("use-{symbol}.c"));
            std::fs::write(
                &source,
                format!("void {symbol}(void);\nint main(void) {{ {symbol}(); return 0; }}\n"),
            )
            .unwrap();
            let object = dir.join(format!("use-{symbol}-{arch}.o"));
            assert!(tool_works(
                "clang",
                &[
                    &format!("--target={arch}-apple-macos13"),
                    "-c",
                    source.to_str().unwrap(),
                    "-o",
                    object.to_str().unwrap(),
                ]
            ));
            let root = syslibroot();
            let result = link_bytes(&os(&[
                "-arch",
                arch,
                "-platform_version",
                "macos",
                "13.0",
                "13.0",
                "-syslibroot",
                root.to_str().unwrap(),
                "-L",
                dir.to_str().unwrap(),
                object.to_str().unwrap(),
                "-lslices",
                "-lSystem",
            ]));
            assert_eq!(result.is_ok(), works, "{arch} {symbol}: {result:?}");
        }
    }
}

/// A universal binary with arm64 and arm64e slices, from universal objects.
#[test]
fn arm64e_universal_binary() {
    if !clang_for("arm64e") || !clang_for("arm64") {
        skip(
            "arm64e_universal_binary",
            "clang cannot target arm64e-apple-macos",
        );
        return;
    }
    let lipo = tool("lipo");
    if !tool_works(&lipo, &["-version"]) {
        skip("arm64e_universal_binary", "llvm-lipo is not installed");
        return;
    }
    let dir = scratch("arm64e_fat");
    let source = super::data_dir().join("hello.c");
    let mut thin = Vec::new();
    for arch in ["arm64", "arm64e"] {
        let object = dir.join(format!("hello-{arch}.o"));
        assert!(tool_works(
            "clang",
            &[
                &format!("--target={arch}-apple-macos13"),
                "-c",
                source.to_str().unwrap(),
                "-o",
                object.to_str().unwrap(),
            ]
        ));
        thin.push(object);
    }
    let fat_object = dir.join("hello-fat.o");
    let status = Command::new(&lipo)
        .arg("-create")
        .args(&thin)
        .arg("-output")
        .arg(&fat_object)
        .status()
        .unwrap();
    assert!(status.success());
    let root = syslibroot();
    let (bytes, _) = link_bytes(&os(&[
        "-arch",
        "arm64",
        "-arch",
        "arm64e",
        "-platform_version",
        "macos",
        "13.0",
        "13.0",
        "-syslibroot",
        root.to_str().unwrap(),
        fat_object.to_str().unwrap(),
        "-lSystem",
    ]))
    .unwrap();
    // fat_header (big-endian): two slices, arm64 then arm64e.
    let be = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    assert_eq!(be(0), 0xcafe_babe);
    assert_eq!(be(4), 2);
    assert_eq!((be(8), be(12)), (0x0100_000c, 0));
    assert_eq!((be(28), be(32)), (0x0100_000c, 0x8000_0002));
    let arm64e_offset = be(36) as usize;
    assert_eq!(arm64e_offset % 0x4000, 0);
    assert_eq!(u32_le(&bytes, arm64e_offset + 8), 0x8000_0002);
}

/// Objective-C on arm64e: method implementations are authenticated
/// pointers in the objects, and relative method lists (with category
/// merging) still reach them, through 32-bit offsets that need no signing.
#[test]
fn arm64e_objective_c() {
    if !clang_for("arm64e") {
        skip(
            "arm64e_objective_c",
            "clang cannot target arm64e-apple-macos",
        );
        return;
    }
    let dir = scratch("arm64e_objc");
    let object = compile_arm64e(&dir, "objc_categories.m", "arm64e-apple-macos13");
    let output = dir.join("categories");
    let root = syslibroot();
    let (bytes, _) = link_bytes(&os(&[
        "-arch",
        "arm64e",
        "-platform_version",
        "macos",
        "13.0",
        "13.0",
        "-syslibroot",
        root.to_str().unwrap(),
        "-objc_category_merging",
        object.to_str().unwrap(),
        "-lobjc",
        "-lSystem",
        "-o",
        output.to_str().unwrap(),
    ]))
    .unwrap();
    let (_, fixups) = walk_chains(&bytes);
    let all = sections(&bytes);
    let text = segments(&bytes)
        .into_iter()
        .find(|s| s.0 == "__TEXT")
        .unwrap();
    let file_offset = |address: u64| (text.3 + (address - text.1)) as usize;
    let string = |address: u64| {
        let at = file_offset(address);
        let end = bytes[at..].iter().position(|&b| b == 0).unwrap();
        String::from_utf8_lossy(&bytes[at..at + end]).into_owned()
    };
    let functions: Vec<(u64, String)> = symbols(&bytes)
        .into_iter()
        .filter(|s| s.is_defined())
        .map(|s| (s.n_value, s.name))
        .collect();
    let (methlist, size) = all
        .iter()
        .find(|s| s.1 == "__objc_methlist")
        .map(|s| (s.2, s.3))
        .expect("__objc_methlist");
    let mut methods = Vec::new();
    let mut at = 0;
    while at < size {
        let header = u32_le(&bytes, file_offset(methlist + at));
        let count = u64::from(u32_le(&bytes, file_offset(methlist + at + 4)));
        assert_eq!(header & 0x8000_ffff, 0x8000_000c);
        for index in 0..count {
            let entry = methlist + at + 8 + index * 12;
            let field = |n: u64| {
                let place = entry + n * 4;
                place.wrapping_add(u32_le(&bytes, file_offset(place)) as i32 as i64 as u64)
            };
            // The selector reference is rebased (plainly) to the name.
            let name = match &fixups[&field(0)] {
                Pointer::Rebase { target, auth: None } => string(*target),
                other => panic!("selector reference {other:?}"),
            };
            let imp = field(2);
            let function = functions
                .iter()
                .find(|(address, name)| *address == imp && name.contains('['))
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| panic!("{name}: no function at {imp:#x}"));
            methods.push(format!("{name} {function}"));
        }
        at += 8 + count * 12;
    }
    methods.sort();
    for wanted in [
        "base -[Base base]",
        "base -[Base(Doubling) base]",
        "first -[NSObject(First) first]",
        "load +[Base(Loading) load]",
        "second +[NSObject(Second) second]",
        "tripleFactor +[Base(Tripling) tripleFactor]",
    ] {
        assert!(
            methods.iter().any(|m| m == wanted),
            "{wanted} in {methods:#?}"
        );
    }
    // One catlist entry for NSObject's merged categories, one for +load.
    let catlist = all.iter().find(|s| s.1 == "__objc_catlist").unwrap();
    assert_eq!(catlist.3, 16);
}
