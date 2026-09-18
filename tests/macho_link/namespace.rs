//! `-force_flat_namespace` (an executable with `MH_FORCE_FLAT`, binding
//! with flat lookup) and `-alias_list` (the aliases of `-alias`, from a
//! file). `ld64.lld` supports neither, so the outputs are checked
//! structurally, and run on macOS.

use qld::macho::read::consts::{MH_FORCE_FLAT, MH_TWOLEVEL};
use qld::macho::read::{MachOFile, Source};

use super::{
    base_args, chained_imports, clang_for, compile, host_can_run, import, link_bytes, objdump, os,
    run, scratch, skip, strings, symbol_address,
};

fn link(args: &[String], output: &std::path::Path) -> Vec<u8> {
    let mut all: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
    all.push("-o".into());
    all.push(output.into());
    let (bytes, _) = link_bytes(&all).unwrap_or_else(|e| panic!("link failed: {e}"));
    std::fs::write(output, &bytes).unwrap();
    super::make_executable(output);
    bytes
}

#[test]
fn force_flat_namespace() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "force_flat_namespace",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let object = compile("force_flat", "hello.c", arch, &[]);
        let exe = scratch("force_flat").join(format!("hello-{arch}"));
        let mut args = base_args(arch);
        args.extend(strings(&[
            "-force_flat_namespace",
            object.to_str().unwrap(),
            "-lSystem",
        ]));
        let bytes = link(&args, &exe);
        let flags = MachOFile::parse(&bytes, Source::new(&exe))
            .unwrap()
            .header()
            .flags;
        assert_eq!(flags & MH_TWOLEVEL, 0, "{flags:#x}");
        assert_eq!(flags & MH_FORCE_FLAT, MH_FORCE_FLAT, "{flags:#x}");
        let imports = chained_imports(&bytes);
        assert_eq!(import(&imports, "_printf").lib_ordinal, -2, "{imports:?}");
        if host_can_run(arch) {
            assert_eq!(run(&exe).unwrap(), "hello from qld 3 42\n");
        }
    }
    let error = link_bytes(&os(&[
        "-arch",
        "arm64",
        "-dylib",
        "-force_flat_namespace",
        "a.o",
    ]))
    .unwrap_err();
    assert!(error.contains("main executables"), "{error}");
}

#[test]
fn alias_list() {
    for arch in ["arm64", "x86_64"] {
        if !clang_for(arch) {
            skip(
                "alias_list",
                &format!("clang cannot target {arch}-apple-macos"),
            );
            continue;
        }
        let dir = scratch("alias_list").join(arch);
        std::fs::create_dir_all(&dir).unwrap();
        let lib = compile("alias_list", "init_lib.c", arch, &[]);
        let list = dir.join("aliases.txt");
        std::fs::write(
            &list,
            "# symbol alias\n_lib_ready _lib_ready_alias\n_lib_ready\t_lib_ready_second\n",
        )
        .unwrap();
        let dylib = dir.join("libalias.dylib");
        let mut args = base_args(arch);
        args.extend(strings(&[
            "-dylib",
            "-install_name",
            "@rpath/libalias.dylib",
            "-alias_list",
            list.to_str().unwrap(),
            lib.to_str().unwrap(),
            "-lSystem",
        ]));
        let bytes = link(&args, &dylib);
        let target = symbol_address(&bytes, "_lib_ready");
        assert!(target.is_some());
        for alias in ["_lib_ready_alias", "_lib_ready_second"] {
            assert_eq!(symbol_address(&bytes, alias), target, "{arch}: {alias}");
        }
        if let Some(trie) = objdump(&["--macho", "--exports-trie"], &dylib) {
            assert!(trie.contains("_lib_ready_second"), "{trie}");
        }
    }
}
