//! Integration tests for `qld::input`: archives written by real `ar` tools,
//! the file table on real files, and library search on a real directory tree.
//!
//! Tests that need an external tool (GNU `ar`, `llvm-ar`) skip with a message
//! when the tool is not installed, so they pass on every CI host.

#![allow(clippy::type_complexity)] // Table-driven test cases.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use qld::args::{InputAttrs, InputKind, InputSpec};
use qld::input::identify::TextKind;
use qld::input::search::apply_sysroot;
use qld::input::{
    Archive, ArchiveKind, FileFormat, FileTable, LibraryNaming, RealFileSystem, SearchContext,
    Source, SymbolIndexKind,
};

/// A scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("qld-input-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    fn write(&self, relative: &str, data: &[u8]) {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Builds a minimal ELF64 x86-64 relocatable object defining `symbols` as
/// global functions in `.text`. Enough for `ar` to index it.
fn elf_object(symbols: &[&str]) -> Vec<u8> {
    fn u16le(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn u32le(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn u64le(out: &mut Vec<u8>, v: u64) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn align(out: &mut Vec<u8>, to: usize) {
        while !out.len().is_multiple_of(to) {
            out.push(0);
        }
    }

    let shstrtab = b"\0.text\0.symtab\0.strtab\0.shstrtab\0";
    let mut strtab = vec![0u8];
    let mut symtab = vec![0u8; 24];
    for symbol in symbols {
        let name = u32::try_from(strtab.len()).unwrap();
        strtab.extend_from_slice(symbol.as_bytes());
        strtab.push(0);
        u32le(&mut symtab, name);
        symtab.push(0x12); // STB_GLOBAL, STT_FUNC
        symtab.push(0);
        u16le(&mut symtab, 1);
        u64le(&mut symtab, 0);
        u64le(&mut symtab, 1);
    }

    let mut out = vec![0u8; 64];
    let text_offset = out.len();
    out.push(0xc3);
    align(&mut out, 8);
    let symtab_offset = out.len();
    out.extend_from_slice(&symtab);
    let strtab_offset = out.len();
    out.extend_from_slice(&strtab);
    let shstrtab_offset = out.len();
    out.extend_from_slice(shstrtab);
    align(&mut out, 8);
    let shoff = out.len();

    // (name, type, flags, offset, size, link, info, align, entsize)
    let sections: [(u32, u32, u64, usize, usize, u32, u32, u64, u64); 5] = [
        (0, 0, 0, 0, 0, 0, 0, 0, 0),
        (1, 1, 6, text_offset, 1, 0, 0, 1, 0),
        (7, 2, 0, symtab_offset, symtab.len(), 3, 1, 8, 24),
        (15, 3, 0, strtab_offset, strtab.len(), 0, 0, 1, 0),
        (23, 3, 0, shstrtab_offset, shstrtab.len(), 0, 0, 1, 0),
    ];
    for (name, kind, flags, offset, size, link, info, align, entsize) in sections {
        u32le(&mut out, name);
        u32le(&mut out, kind);
        u64le(&mut out, flags);
        u64le(&mut out, 0);
        u64le(&mut out, offset as u64);
        u64le(&mut out, size as u64);
        u32le(&mut out, link);
        u32le(&mut out, info);
        u64le(&mut out, align);
        u64le(&mut out, entsize);
    }

    let mut header = Vec::new();
    header.extend_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    u16le(&mut header, 1); // ET_REL
    u16le(&mut header, 62); // EM_X86_64
    u32le(&mut header, 1);
    u64le(&mut header, 0);
    u64le(&mut header, 0);
    u64le(&mut header, shoff as u64);
    u32le(&mut header, 0);
    u16le(&mut header, 64);
    u16le(&mut header, 0);
    u16le(&mut header, 0);
    u16le(&mut header, 64);
    u16le(&mut header, 5);
    u16le(&mut header, 4);
    out[..64].copy_from_slice(&header);
    out
}

/// The members every tool test archives, with the symbols each defines.
fn fixture_members() -> Vec<(&'static str, Vec<u8>, Vec<&'static str>)> {
    let mut big = b"large member, maps the archive\n".repeat(700);
    big.push(b'!'); // odd size, exercises padding
    vec![
        ("a.o", elf_object(&["foo", "bar"]), vec!["foo", "bar"]),
        (
            "a_member_with_a_long_name.o",
            elf_object(&["long_function"]),
            vec!["long_function"],
        ),
        ("notes.txt", b"odd".to_vec(), vec![]),
        ("big.bin", big, vec![]),
    ]
}

fn find_llvm_ar() -> Option<PathBuf> {
    if tool_works(Path::new("llvm-ar"), &["--version"]) {
        return Some(PathBuf::from("llvm-ar"));
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir("/usr/lib/llvm")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("bin/llvm-ar"))
        .filter(|path| path.is_file())
        .collect();
    candidates.sort();
    candidates.pop()
}

fn find_gnu_ar() -> Option<PathBuf> {
    let output = Command::new("ar").arg("--version").output().ok()?;
    String::from_utf8_lossy(&output.stdout)
        .contains("GNU")
        .then(|| PathBuf::from("ar"))
}

fn tool_works(tool: &Path, args: &[&str]) -> bool {
    Command::new(tool)
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Runs `tool args... archive members...` in `dir`.
fn run_ar(
    tool: &Path,
    dir: &Path,
    env: &[(&str, &str)],
    args: &[&str],
    archive: &str,
    members: &[String],
) {
    let status = Command::new(tool)
        .current_dir(dir)
        .envs(env.iter().copied())
        .args(args)
        .arg(archive)
        .args(members)
        .status()
        .unwrap();
    assert!(status.success(), "{} {args:?} failed", tool.display());
}

/// Checks an archive written by a tool from [`fixture_members`], stored under
/// `dir/obj/`, against what went in.
fn check_archive(
    dir: &Path,
    archive_rel: &str,
    expect_index: bool,
    expect_kind: Option<ArchiveKind>,
) {
    let fixtures = fixture_members();
    let archive_path = dir.join(archive_rel);
    let table = FileTable::new();
    let id = table.load_path(&archive_path).unwrap();
    let file = table.get(id).unwrap();
    let archive = file
        .archive()
        .unwrap_or_else(|e| panic!("{archive_rel}: {e}"));
    assert!(matches!(
        file.format(),
        FileFormat::Archive | FileFormat::ThinArchive
    ));
    assert_eq!(archive.is_thin(), file.format() == FileFormat::ThinArchive);
    if let Some(kind) = expect_kind {
        assert_eq!(archive.kind(), kind, "{archive_rel}");
    }

    let members: Vec<_> = archive
        .members()
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| panic!("{archive_rel}: {e}"));
    assert_eq!(members.len(), fixtures.len(), "{archive_rel}");
    let mut by_offset = BTreeMap::new();
    for (member, (name, data, _)) in members.iter().zip(&fixtures) {
        let member_name = String::from_utf8_lossy(member.name);
        assert!(
            member_name == *name || member_name.ends_with(&format!("/{name}")),
            "{archive_rel}: member {member_name} is not {name}"
        );
        let member_id = table.add_member(id, member).unwrap();
        let entry = table.get(member_id).unwrap();
        if archive.kind() == ArchiveKind::Bsd {
            // Darwin-style writers align members to 8 bytes with `\n` and
            // count the padding in the member size.
            let (contents, padding) = entry.data().split_at(data.len().min(entry.data().len()));
            assert_eq!(contents, &data[..], "{archive_rel}: {name}");
            assert!(padding.len() < 8 && padding.iter().all(|&b| b == b'\n'));
        } else {
            assert_eq!(entry.data(), &data[..], "{archive_rel}: {name}");
        }
        assert_eq!(entry.parent(), Some(id));
        if name.ends_with(".o") {
            assert!(matches!(entry.format(), FileFormat::Elf(_)), "{name}");
        }
        if archive.is_thin() {
            assert!(member.bytes().is_none());
            assert_eq!(member.size, data.len() as u64);
        }
        by_offset.insert(member.header_offset, *name);
    }

    let Some(index) = archive.symbol_index() else {
        assert!(!expect_index, "{archive_rel}: no symbol index");
        return;
    };
    assert!(expect_index, "{archive_rel}: unexpected symbol index");
    let mut found = BTreeMap::new();
    for symbol in index.iter() {
        let symbol = symbol.unwrap_or_else(|e| panic!("{archive_rel}: {e}"));
        let member = archive.member_at(symbol.member_offset).unwrap();
        assert!(by_offset.contains_key(&member.header_offset));
        found.insert(
            String::from_utf8(symbol.name.to_vec()).unwrap(),
            by_offset[&symbol.member_offset],
        );
    }
    let mut expected = BTreeMap::new();
    for (name, _, symbols) in &fixtures {
        for symbol in symbols {
            expected.insert(symbol.to_string(), *name);
        }
    }
    assert_eq!(found, expected, "{archive_rel}");
}

fn write_fixtures(scratch: &Scratch) -> Vec<String> {
    fixture_members()
        .iter()
        .map(|(name, data, _)| {
            scratch.write(&format!("obj/{name}"), data);
            format!("obj/{name}")
        })
        .collect()
}

#[test]
fn gnu_ar_archives() {
    let Some(ar) = find_gnu_ar() else {
        eprintln!("skipping: GNU ar not found");
        return;
    };
    let scratch = Scratch::new("gnu-ar");
    let members = write_fixtures(&scratch);
    std::fs::create_dir_all(scratch.path("lib")).unwrap();
    run_ar(&ar, &scratch.0, &[], &["rcs"], "lib/libgnu.a", &members);
    check_archive(&scratch.0, "lib/libgnu.a", true, Some(ArchiveKind::Gnu));
    run_ar(&ar, &scratch.0, &[], &["rcS"], "lib/libnoindex.a", &members);
    check_archive(
        &scratch.0,
        "lib/libnoindex.a",
        false,
        Some(ArchiveKind::Gnu),
    );
    run_ar(&ar, &scratch.0, &[], &["rcsT"], "lib/libthin.a", &members);
    check_archive(&scratch.0, "lib/libthin.a", true, Some(ArchiveKind::Gnu));
    let data = std::fs::read(scratch.path("lib/libgnu.a")).unwrap();
    corrupt_and_parse(&data);
}

#[test]
fn llvm_ar_archives() {
    let Some(ar) = find_llvm_ar() else {
        eprintln!("skipping: llvm-ar not found");
        return;
    };
    let scratch = Scratch::new("llvm-ar");
    let members = write_fixtures(&scratch);
    std::fs::create_dir_all(scratch.path("lib")).unwrap();
    let sym64 = [("SYM64_THRESHOLD", "0")];
    let cases: &[(
        &str,
        &[(&str, &str)],
        &[&str],
        Option<ArchiveKind>,
        Option<SymbolIndexKind>,
    )] = &[
        (
            "libgnu.a",
            &[],
            &["rcs", "--format=gnu"],
            Some(ArchiveKind::Gnu),
            Some(SymbolIndexKind::Sysv32),
        ),
        (
            "libgnu64.a",
            &sym64,
            &["rcs", "--format=gnu"],
            Some(ArchiveKind::Gnu),
            Some(SymbolIndexKind::Sysv64),
        ),
        (
            "libbsd.a",
            &[],
            &["rcs", "--format=bsd"],
            Some(ArchiveKind::Bsd),
            Some(SymbolIndexKind::Bsd32 { big_endian: false }),
        ),
        (
            "libdarwin.a",
            &[],
            &["rcs", "--format=darwin"],
            Some(ArchiveKind::Bsd),
            Some(SymbolIndexKind::Bsd32 { big_endian: false }),
        ),
        (
            "libdarwin64.a",
            &sym64,
            &["rcs", "--format=darwin"],
            Some(ArchiveKind::Bsd),
            Some(SymbolIndexKind::Bsd64 { big_endian: false }),
        ),
        (
            "libcoff.a",
            &[],
            &["rcs", "--format=coff"],
            Some(ArchiveKind::Coff),
            Some(SymbolIndexKind::Sysv32),
        ),
        (
            "libthin.a",
            &[],
            &["rcs", "--thin"],
            Some(ArchiveKind::Gnu),
            Some(SymbolIndexKind::Sysv32),
        ),
        ("libnoindex.a", &[], &["rcS", "--format=gnu"], None, None),
        (
            "libbsdnoindex.a",
            &[],
            &["rcS", "--format=bsd"],
            Some(ArchiveKind::Bsd),
            None,
        ),
    ];
    for (name, env, args, kind, index_kind) in cases {
        let rel = format!("lib/{name}");
        run_ar(&ar, &scratch.0, env, args, &rel, &members);
        check_archive(&scratch.0, &rel, index_kind.is_some(), *kind);
        let data = std::fs::read(scratch.path(&rel)).unwrap();
        let archive = Archive::parse(Path::new(name), &data).unwrap();
        assert_eq!(
            archive.symbol_index().map(|i| i.kind()),
            *index_kind,
            "{name}"
        );
    }
    let data = std::fs::read(scratch.path("lib/libdarwin.a")).unwrap();
    corrupt_and_parse(&data);
    let data = std::fs::read(scratch.path("lib/libthin.a")).unwrap();
    corrupt_and_parse(&data);
}

/// Truncates and flips bytes of `data` deterministically, parsing each
/// variant. Only a panic fails the test.
fn corrupt_and_parse(data: &[u8]) {
    let exercise = |bytes: &[u8]| {
        let Ok(archive) = Archive::parse(Path::new("corrupt.a"), bytes) else {
            return;
        };
        for member in archive.members().take(100) {
            if member.is_err() {
                break;
            }
        }
        if let Some(index) = archive.symbol_index() {
            for symbol in index.iter().take(100) {
                let Ok(symbol) = symbol else { break };
                let _ = archive.member_at(symbol.member_offset);
            }
        }
        let _ = qld::input::identify(bytes);
    };
    // Every prefix of the headers, then sampled prefixes of the rest.
    let step = (data.len() / 512).max(1);
    for len in (0..data.len().min(2048)).chain((2048..data.len()).step_by(step)) {
        exercise(&data[..len]);
    }
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let hot = data.len().min(1024);
    for _ in 0..2000 {
        let mut copy = data.to_vec();
        for _ in 0..(next() % 3 + 1) {
            // Mostly corrupt the headers and index, where the parsing is.
            let at = if next() % 4 == 0 {
                (next() % data.len() as u64) as usize
            } else {
                (next() % hot as u64) as usize
            };
            copy[at] ^= 1 << (next() % 8);
        }
        exercise(&copy);
    }
}

#[test]
fn file_table_maps_and_slices_members() {
    let scratch = Scratch::new("table");
    let big = vec![0x5au8; 100_000];
    scratch.write("big.bin", &big);
    scratch.write("small.ld", b"GROUP ( libc.so.6 )\n");
    let table = FileTable::new();
    let sources = vec![
        Source::Path(scratch.path("big.bin")),
        Source::Path(scratch.path("missing.o")),
        Source::Path(scratch.path("small.ld")),
        Source::Bytes {
            name: "memory.o".into(),
            data: Arc::from(elf_object(&["x"])),
        },
    ];
    let results = table.load_all(&sources);
    let big_id = *results[0].as_ref().unwrap();
    assert!(results[1].is_err());
    let script_id = *results[2].as_ref().unwrap();
    let memory_id = *results[3].as_ref().unwrap();
    assert_eq!(
        (big_id.index(), script_id.index(), memory_id.index()),
        (0, 1, 2)
    );
    assert_eq!(table.data(big_id), &big[..]);
    #[cfg(target_os = "linux")]
    assert!(table.get(big_id).unwrap().is_mapped());
    assert_eq!(
        table.get(script_id).unwrap().format(),
        FileFormat::Text(TextKind::Other)
    );
    assert!(!table.get(script_id).unwrap().is_mapped());
    assert!(matches!(
        table.get(memory_id).unwrap().format(),
        FileFormat::Elf(ident) if ident.is_relocatable()
    ));

    // Errors name the file.
    let error = results[1].as_ref().unwrap_err().to_string();
    assert!(error.contains("missing.o"), "{error}");
}

#[test]
fn parallel_loading_is_deterministic() {
    let scratch = Scratch::new("parallel");
    let sources: Vec<Source> = (0..200)
        .map(|i| {
            let name = format!("f{i}.bin");
            scratch.write(
                &name,
                format!("contents of file {i}")
                    .repeat(i * 20 + 1)
                    .as_bytes(),
            );
            Source::Path(scratch.path(&name))
        })
        .collect();
    let first: Vec<_> = {
        let table = FileTable::new();
        table
            .load_all(&sources)
            .into_iter()
            .map(|r| r.unwrap())
            .map(|id| (id, table.data(id).to_vec()))
            .collect()
    };
    for threads in [1, 3, 8] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let table = FileTable::new();
        let again: Vec<_> = pool.install(|| {
            table
                .load_all(&sources)
                .into_iter()
                .map(|r| r.unwrap())
                .map(|id| (id, table.data(id).to_vec()))
                .collect()
        });
        assert_eq!(first, again, "{threads} threads");
    }
}

#[test]
fn library_search_on_disk() {
    let scratch = Scratch::new("search");
    scratch.write("sysroot/usr/lib/libc.so", b"GROUP ( libc.so.6 )\n");
    scratch.write("sysroot/usr/lib/libc.a", b"!<arch>\n");
    scratch.write("local/libm.a", b"!<arch>\n");
    scratch.write("local/crt1.o", &elf_object(&["_start"]));
    std::fs::create_dir_all(scratch.path("local/libdir.so")).unwrap();

    let sysroot = scratch.path("sysroot");
    let search_paths = vec![PathBuf::from("=/usr/lib"), scratch.path("local")];
    let context = SearchContext {
        search_paths: &search_paths,
        sysroot: Some(&sysroot),
        naming: LibraryNaming::Elf,
        fs: &RealFileSystem,
    };
    let spec = |kind, static_only| {
        let mut attrs = InputAttrs::default();
        attrs.static_only = static_only;
        InputSpec::new(kind, attrs)
    };
    let resolve = |kind, static_only| match context.resolve(&spec(kind, static_only)) {
        Ok(Source::Path(path)) => Some(path),
        Ok(other) => panic!("unexpected {other:?}"),
        Err(_) => None,
    };
    let sysroot_lib = apply_sysroot(Path::new("=/usr/lib"), Some(&sysroot));
    assert_eq!(
        resolve(InputKind::Library("c".into()), false),
        Some(sysroot_lib.join("libc.so"))
    );
    assert_eq!(
        resolve(InputKind::Library("c".into()), true),
        Some(sysroot_lib.join("libc.a"))
    );
    assert_eq!(
        resolve(InputKind::Library("m".into()), false),
        Some(scratch.path("local").join("libm.a"))
    );
    assert_eq!(
        resolve(InputKind::LibraryExact("crt1.o".into()), false),
        Some(scratch.path("local").join("crt1.o"))
    );
    // A directory named like a library is not a match.
    assert_eq!(resolve(InputKind::Library("dir".into()), false), None);

    // Resolved paths load and identify.
    let table = FileTable::new();
    let path = resolve(InputKind::Library("c".into()), false).unwrap();
    let id = table.load_path(&path).unwrap();
    assert_eq!(
        table.get(id).unwrap().format(),
        FileFormat::Text(TextKind::Other)
    );
}

/// Parses every static library under the usual system directories. Slow and
/// host-dependent, so it only runs on request:
/// `cargo test --test input -- --ignored`.
#[test]
#[ignore]
fn system_archive_sweep() {
    let mut archives = Vec::new();
    let mut stack: Vec<PathBuf> = ["/usr/lib", "/usr/lib64", "/lib", "/usr/local/lib"]
        .iter()
        .map(PathBuf::from)
        .collect();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() && path.extension().is_some_and(|e| e == "a") {
                archives.push(path);
            }
        }
    }
    archives.sort();
    let table = FileTable::new();
    let sources: Vec<Source> = archives.iter().cloned().map(Source::Path).collect();
    let mut failures = Vec::new();
    let mut symbols = 0usize;
    for (path, id) in archives.iter().zip(table.load_all(&sources)) {
        let Ok(id) = id else { continue };
        let file = table.get(id).unwrap();
        if !file.format().is_archive() {
            continue;
        }
        let result = file.archive().and_then(|archive| {
            for member in archive.members() {
                member?;
            }
            if let Some(index) = archive.symbol_index() {
                for symbol in index.iter() {
                    archive.member_at(symbol?.member_offset)?;
                    symbols += 1;
                }
            }
            Ok(())
        });
        if let Err(error) = result {
            failures.push(format!("{}: {error}", path.display()));
        }
    }
    eprintln!(
        "parsed {} archives, {symbols} index entries",
        archives.len()
    );
    assert!(failures.is_empty(), "{failures:#?}");
}
