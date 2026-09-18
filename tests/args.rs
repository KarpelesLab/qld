//! Command-line parsing tests (workstream W1).
//!
//! Every entry of the option table and the `-z` keyword table is exercised in
//! every spelling it accepts, followed by tests for each syntax rule, the
//! positional state, response files, flavor selection, and real command lines
//! captured from gcc, clang and rustc (in `tests/data/args/`).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use qld::args::table::{self, ArgKind, GNU_OPTIONS, Status, Z_KEYWORDS, ZArg};
use qld::args::{
    BuildId, ColorChoice, DebugCompression, DiscardMode, ExecStack, Flavor, HashStyle, IcfMode,
    InputAttrs, InputFormat, InputKind, LinkOptions, MagicMode, OrphanHandling, OutputFormat,
    OutputKind, ParseOutcome, SeparateCode, SortSection, StripMode, SymbolicMode, Visibility,
    parse_gnu_with, select_flavor,
};
use qld::{Architecture, BinaryFormat, Endianness, Error, Target};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A reader with no files: parsing must not touch the file system.
fn no_files(path: &Path) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("unexpected read of {}", path.display()),
    ))
}

fn parse(args: &[&str]) -> qld::Result<ParseOutcome> {
    let mut argv = vec!["ld"];
    argv.extend_from_slice(args);
    parse_gnu_with(&argv, &no_files)
}

fn link(args: &[&str]) -> Box<LinkOptions> {
    match parse(args) {
        Ok(ParseOutcome::Link(options)) => options,
        Ok(other) => panic!("{args:?}: expected a link, got {other:?}"),
        Err(error) => panic!("{args:?}: {error}"),
    }
}

fn error(args: &[&str]) -> String {
    match parse(args) {
        Err(Error::Option(message)) => message,
        Err(other) => panic!("{args:?}: expected an option error, got {other:?}"),
        Ok(outcome) => panic!("{args:?}: expected an error, got {outcome:?}"),
    }
}

/// `Debug` rendering without the `ignored` list, to compare parse results
/// that differ only in spelling.
fn render(options: &LinkOptions) -> String {
    let mut options = options.clone();
    options.ignored.clear();
    format!("{options:?}")
}

fn file(path: &str) -> InputKind {
    InputKind::File(PathBuf::from(path))
}

fn lib(name: &str) -> InputKind {
    InputKind::Library(name.to_owned())
}

fn attrs_of<'a>(options: &'a LinkOptions, kind: &InputKind) -> &'a InputAttrs {
    &options
        .inputs
        .iter()
        .find(|input| &input.kind == kind)
        .unwrap_or_else(|| panic!("no input {kind:?} in {:?}", options.inputs))
        .attrs
}

// ---------------------------------------------------------------------------
// Table-driven coverage of every option
// ---------------------------------------------------------------------------

/// A valid value for options that validate theirs.
fn sample(name: &str) -> &'static str {
    match name {
        "m" => "elf_x86_64",
        "b" | "format" => "binary",
        "a" => "archive",
        "build-id" => "md5",
        "hash-style" => "both",
        "icf" => "all",
        "O" | "lto-O" | "lto-CGO" => "2",
        "threads" | "thread-count" | "error-limit" | "spare-dynamic-tags" => "4",
        "image-base" => "0x400000",
        "Ttext" | "Tdata" | "Tbss" | "Ttext-segment" | "Trodata-segment" | "Tldata-segment" => {
            "1000"
        }
        "section-start" => ".foo=0x1000",
        "defsym" => "sym=0x10",
        "unresolved-symbols" => "ignore-all",
        "orphan-handling" => "warn",
        "sort-section" => "alignment",
        "compress-debug-sections" => "zlib",
        "call-graph-profile-sort" => "hfsort",
        "color-diagnostics" => "never",
        "pack-dyn-relocs" => "relr",
        "z" => "now",
        "rsp-quoting" => "posix",
        "demangle" => "auto",
        "subsystem" => "windows",
        "stack" => "0x100000,0x1000",
        "heap" => "0x200000,0x2000",
        "section-alignment" => "0x2000",
        "file-alignment" => "0x400",
        "major-image-version"
        | "minor-image-version"
        | "major-os-version"
        | "minor-os-version"
        | "major-subsystem-version"
        | "minor-subsystem-version" => "3",
        _ => "value",
    }
}

/// Arguments that must precede an option for it to be valid.
fn prefix(name: &str) -> &'static [&'static str] {
    match name {
        "end-group" | ")" => &["--start-group"],
        "end-lib" => &["--start-lib"],
        "pop-state" => &["--push-state"],
        "plugin-opt" => &["-plugin", "plugin.so"],
        _ => &[],
    }
}

/// Implemented options whose sample use leaves `LinkOptions` at its default.
const SETS_DEFAULT: &[&str] = &[
    "rsp-quoting",
    "no-whole-archive",
    "no-as-needed",
    "Bdynamic",
    "dy",
    "call_shared",
    "no-copy-dt-needed-entries",
    "no-add-needed",
    "push-state",
    "pop-state",
    "end-group",
    ")",
    "end-lib",
    "no-pie",
    "no-eh-frame-hdr",
    "error-unresolved-symbols",
    "no-allow-multiple-definition",
    "no-gc-sections",
    "no-relax-gp",
    "no-print-gc-sections",
    "no-print-icf-sections",
    "no-omagic",
    "no-nmagic",
    "relax",
    "no-apply-dynamic-relocs",
    "Bno-symbolic",
    "no-export-dynamic",
    "no-warn-common",
    "no-warn-backrefs",
    "no-fatal-warnings",
    "demangle",
    "dependent-libraries",
    "fork",
    "no-call-graph-profile-sort",
    "warn-symbol-ordering",
    "no-gdb-index",
    "no-debug-names",
    "no-separate-debug-file",
    // PE/COFF options whose value is the GNU ld `i386pep` default.
    "dynamicbase",
    "nxcompat",
    "high-entropy-va",
    "large-address-aware",
    "disable-tsaware",
    "disable-no-seh",
    "disable-forceinteg",
    "disable-no-isolation",
    "disable-no-bind",
    "disable-wdmdriver",
    "enable-reloc-section",
    "no-insert-timestamp",
    "enable-auto-import",
    "enable-runtime-pseudo-reloc",
    "enable-runtime-pseudo-reloc-v2",
];

/// Every way an option can be written, each as a list of argv entries.
fn spellings(name: &str, arg: ArgKind) -> Vec<Vec<String>> {
    let value = sample(name);
    let mut dashes = vec!["--"];
    if name.len() == 1 {
        dashes = vec!["-"];
    } else if !name.starts_with('o') {
        dashes.push("-");
    }
    let mut forms = Vec::new();
    for dash in dashes {
        let opt = format!("{dash}{name}");
        match (arg, name.len()) {
            (ArgKind::Flag, _) => forms.push(vec![opt]),
            (ArgKind::Value, 1) => {
                forms.push(vec![format!("{opt}{value}")]);
                forms.push(vec![opt, value.to_owned()]);
            }
            (ArgKind::Value, _) => {
                forms.push(vec![format!("{opt}={value}")]);
                forms.push(vec![opt, value.to_owned()]);
            }
            (ArgKind::OptionalValue, _) => {
                forms.push(vec![opt.clone()]);
                forms.push(vec![format!("{opt}={value}")]);
            }
            (ArgKind::EqualsValue, _) => forms.push(vec![format!("{opt}={value}")]),
            (ArgKind::JoinedValue, _) => forms.push(vec![format!("{opt}{value}")]),
        }
    }
    forms
}

#[test]
fn every_option_in_every_spelling() {
    for def in GNU_OPTIONS {
        let before: Vec<&str> = prefix(def.name).to_vec();
        let mut baseline_args = before.clone();
        baseline_args.push("in.o");
        let baseline = render(&link(&baseline_args));

        let forms = spellings(def.name, def.arg);
        assert!(!forms.is_empty(), "{}", def.name);
        let mut first_result: Option<String> = None;
        for form in &forms {
            let mut args = before.clone();
            args.extend(form.iter().map(String::as_str));
            args.push("in.o");
            let what = format!("{args:?}");
            match def.status {
                Status::Implemented => match parse(&args) {
                    Ok(ParseOutcome::Link(options)) => {
                        let rendered = render(&options);
                        // The optional-value form without a value may differ
                        // from the form with one; everything else must agree.
                        if def.arg != ArgKind::OptionalValue {
                            match &first_result {
                                Some(first) => assert_eq!(first, &rendered, "{what}"),
                                None => first_result = Some(rendered.clone()),
                            }
                        }
                        if def.arg != ArgKind::OptionalValue && !SETS_DEFAULT.contains(&def.name) {
                            assert_ne!(rendered, baseline, "{what} had no effect");
                        }
                        assert!(
                            options.warnings.is_empty(),
                            "{what}: {:?}",
                            options.warnings
                        );
                        assert!(options.ignored.is_empty(), "{what}: {:?}", options.ignored);
                    }
                    Ok(ParseOutcome::Help | ParseOutcome::Version) => {}
                    Err(error) => panic!("{what}: {error}"),
                },
                Status::Ignored => {
                    let options = link(&args);
                    assert_eq!(render(&options), baseline, "{what} must have no effect");
                    let written: Vec<OsString> = form.iter().map(OsString::from).collect();
                    assert_eq!(options.ignored, written, "{what}");
                }
                Status::Unsupported(why) => {
                    let message = error(&args);
                    assert!(
                        message.contains(def.name) && message.contains(why),
                        "{what}: {message}"
                    );
                }
            }
        }

        if def.name.len() == 1 {
            // Single-letter options never take two dashes.
            let message = error(&[&format!("--{}", def.name), "in.o"]);
            assert!(message.starts_with("unknown option"), "{message}");
        } else if def.name.starts_with('o') && def.arg == ArgKind::Flag {
            // `-omagic` is `-o magic`.
            let options = link(&[&format!("-{}", def.name), "in.o"]);
            assert_eq!(options.output, Some(PathBuf::from(&def.name[1..])));
        }
    }
}

fn z_sample(name: &str) -> &'static str {
    match name {
        "max-page-size" | "common-page-size" => "0x1000",
        "stack-size" => "0x100000",
        "start-stop-visibility" => "hidden",
        "cet-report" => "error",
        "dead-reloc-in-nonalloc" => ".debug_*=0xffffffff",
        _ => "value",
    }
}

const Z_SETS_DEFAULT: &[&str] = &[
    "lazy",
    "relro",
    "copyreloc",
    "combreloc",
    "nopack-relative-relocs",
    "notext",
    "textoff",
    "nounique",
    "nokeep-text-section-prefix",
    "nomark-plt",
    "sectionheader",
    "nomemory-seal",
    "execstack-if-needed",
];

#[test]
fn every_z_keyword_in_both_spellings() {
    let baseline = render(&link(&["in.o"]));
    for z in Z_KEYWORDS {
        let keyword = match z.arg {
            ZArg::Flag => z.name.to_owned(),
            ZArg::Value => format!("{}={}", z.name, z_sample(z.name)),
        };
        let joined = format!("-z{keyword}");
        let forms: [Vec<&str>; 2] = [vec!["-z", &keyword, "in.o"], vec![&joined, "in.o"]];
        for args in &forms {
            let what = format!("{args:?}");
            match z.status {
                Status::Implemented => {
                    let options = link(args);
                    assert!(
                        options.warnings.is_empty(),
                        "{what}: {:?}",
                        options.warnings
                    );
                    assert!(options.ignored.is_empty(), "{what}");
                    if !Z_SETS_DEFAULT.contains(&z.name) {
                        assert_ne!(render(&options), baseline, "{what} had no effect");
                    }
                }
                Status::Ignored => {
                    let options = link(args);
                    assert_eq!(render(&options), baseline, "{what}");
                    assert_eq!(
                        options.ignored,
                        [OsString::from("-z"), keyword.clone().into()]
                    );
                }
                Status::Unsupported(why) => {
                    let message = error(args);
                    assert!(
                        message.contains(&format!("-z {}", z.name)) && message.contains(why),
                        "{what}: {message}"
                    );
                }
            }
        }
        // The wrong value shape is an unknown keyword, which only warns.
        let wrong = match z.arg {
            ZArg::Flag => format!("{}=1", z.name),
            ZArg::Value => z.name.to_owned(),
        };
        let options = link(&["-z", &wrong, "in.o"]);
        assert_eq!(options.warnings, [format!("-z {wrong} ignored")]);
    }
}

#[test]
fn table_lookup_functions() {
    assert_eq!(table::find("soname").map(|d| d.arg), Some(ArgKind::Value));
    assert!(table::find("no-such-option").is_none());
    assert_eq!(table::find_z("now").map(|z| z.arg), Some(ZArg::Flag));
    assert!(table::find_z("no-such-keyword").is_none());
}

#[test]
fn help_lists_every_implemented_option() {
    let help = qld::args::usage();
    for def in GNU_OPTIONS {
        let listed = help.contains(&format!("-{}", def.name));
        if def.status == Status::Implemented {
            assert!(listed, "--help is missing {}", def.name);
        }
    }
    for z in Z_KEYWORDS
        .iter()
        .filter(|z| z.status == Status::Implemented)
    {
        assert!(help.contains(&format!("-z {}", z.name)), "-z {}", z.name);
    }
}

// ---------------------------------------------------------------------------
// Syntax rules
// ---------------------------------------------------------------------------

#[test]
fn long_options_take_one_or_two_dashes() {
    for args in [
        ["-soname", "libx.so"],
        ["--soname", "libx.so"],
        ["-soname=libx.so", "a.o"],
        ["--soname=libx.so", "a.o"],
    ] {
        let mut args = args.to_vec();
        args.push("a.o");
        assert_eq!(link(&args).soname.as_deref(), Some("libx.so"), "{args:?}");
    }
}

#[test]
fn options_starting_with_o_need_two_dashes() {
    let options = link(&["-omagic", "a.o"]);
    assert_eq!(options.output, Some(PathBuf::from("magic")));
    assert_eq!(options.magic, MagicMode::Normal);

    let options = link(&["--omagic", "a.o"]);
    assert_eq!(options.output, None);
    assert_eq!(options.magic, MagicMode::Omagic);

    let options = link(&["-output=x", "a.o"]);
    assert_eq!(options.output, Some(PathBuf::from("utput=x")));
    assert_eq!(
        link(&["--output=x", "a.o"]).output,
        Some(PathBuf::from("x"))
    );
    assert_eq!(
        link(&["--output", "x", "a.o"]).output,
        Some(PathBuf::from("x"))
    );
    assert_eq!(
        link(&["-oformat", "a.o"]).output,
        Some(PathBuf::from("format"))
    );
}

#[test]
fn values_can_be_joined_or_separate() {
    for args in [
        vec!["-Lpath", "-lc"],
        vec!["-L", "path", "-l", "c"],
        vec!["--library-path=path", "--library=c"],
        vec!["--library-path", "path", "--library", "c"],
        vec!["-library-path", "path", "-library", "c"],
    ] {
        let options = link(&args);
        assert_eq!(options.search_paths, [PathBuf::from("path")], "{args:?}");
        assert_eq!(options.inputs.len(), 1);
        assert_eq!(options.inputs[0].kind, lib("c"), "{args:?}");
    }
}

#[test]
fn library_exact_names() {
    let options = link(&["-l:libfoo.a", "-l", ":libbar.so.1", "--library=:libbaz.a"]);
    let kinds: Vec<_> = options.inputs.iter().map(|i| i.kind.clone()).collect();
    assert_eq!(
        kinds,
        [
            InputKind::LibraryExact("libfoo.a".into()),
            InputKind::LibraryExact("libbar.so.1".into()),
            InputKind::LibraryExact("libbaz.a".into()),
        ]
    );
    assert!(error(&["-l", ""]).contains("missing library name"));
}

#[test]
fn z_keywords_joined_separate_and_unknown() {
    let options = link(&[
        "-z",
        "now",
        "-zrelro",
        "-z",
        "max-page-size=0x200000",
        "a.o",
    ]);
    assert!(options.bind_now);
    assert!(options.relro);
    assert_eq!(options.max_page_size, Some(0x20_0000));

    let options = link(&["-z", "bogus", "-zalso-bogus=1", "a.o"]);
    assert_eq!(
        options.warnings,
        ["-z bogus ignored", "-z also-bogus=1 ignored"]
    );

    assert!(error(&["-z", "max-page-size=3", "a.o"]).contains("max-page-size"));
    assert!(error(&["-z", "cet-report=loud", "a.o"]).contains("cet-report"));
    assert!(error(&["-z"]).contains("missing argument to -z"));

    let options = link(&["-z", "norelro", "-z", "lazy", "-z", "nocopyreloc", "a.o"]);
    assert!(!options.relro && !options.bind_now && !options.copy_relocs);
}

#[test]
fn unknown_and_malformed_options() {
    assert_eq!(error(&["--foo", "a.o"]), "unknown option: --foo");
    assert_eq!(error(&["-qux", "a.o"]), "unknown option: -qux");
    assert_eq!(error(&["--f", "a.o"]), "unknown option: --f");
    assert_eq!(error(&["-sX", "a.o"]), "unknown option: -sX");
    assert_eq!(
        error(&["--whole-archive=yes", "a.o"]),
        "option does not take a value: --whole-archive"
    );
    assert_eq!(error(&["a.o", "-o"]), "missing argument to -o");
    assert_eq!(error(&["a.o", "--soname"]), "missing argument to --soname");
    assert_eq!(
        error(&["a.o", "--why-extract"]),
        "missing argument to --why-extract"
    );
    assert_eq!(error(&["-o", "out"]), "no input files");
    assert!(error(&["--icf=sometimes", "a.o"]).contains("--icf"));
    assert!(error(&["--hash-style=fast", "a.o"]).contains("--hash-style"));
    assert!(error(&["--build-id=sha256", "a.o"]).contains("sha256"));
    assert!(error(&["--build-id=0xabc", "a.o"]).contains("0xabc"));
    assert!(error(&["--defsym", "novalue", "a.o"]).contains("SYMBOL=EXPRESSION"));
    assert!(error(&["--section-start", ".text", "a.o"]).contains("SECTION=ADDRESS"));
    assert!(error(&["-Ttext", "xyz", "a.o"]).contains("-Ttext"));
    assert!(error(&["--threads=0", "a.o"]).contains("--threads"));
    assert!(error(&["-O", "fast", "a.o"]).contains("-O"));
    assert!(error(&["-b", "ihex", "a.o"]).contains("ihex"));
    assert!(error(&["-a", "sometimes", "a.o"]).contains("sometimes"));
    assert!(error(&["--pack-dyn-relocs=android", "a.o"]).contains("unsupported"));
    assert!(error(&["--unresolved-symbols=maybe", "a.o"]).contains("maybe"));
    assert!(error(&["--orphan-handling=keep", "a.o"]).contains("keep"));
    assert!(error(&["--color-diagnostics=rainbow", "a.o"]).contains("rainbow"));
    assert!(error(&["--compress-debug-sections=lz4", "a.o"]).contains("lz4"));
    assert!(error(&["--start-lib", "--start-lib", "a.o"]).contains("nested"));
}

#[test]
fn unsupported_options_name_themselves() {
    let message = error(&["--incremental", "a.o"]);
    assert_eq!(
        message,
        "unsupported option: --incremental (incremental linking is not supported)"
    );
    assert!(error(&["-base-file", "x.base", "a.o"]).starts_with("unsupported option: -base-file"));
    assert!(
        error(&["-z", "retpolineplt", "a.o"]).starts_with("unsupported option: -z retpolineplt")
    );
}

#[test]
fn double_dash_ends_options() {
    let options = link(&["a.o", "--", "-lc", "--whole-archive", "-"]);
    let kinds: Vec<_> = options.inputs.iter().map(|i| i.kind.clone()).collect();
    assert_eq!(
        kinds,
        [file("a.o"), file("-lc"), file("--whole-archive"), file("-")]
    );
    // A lone "-" is an input even before "--".
    assert_eq!(link(&["-"]).inputs[0].kind, file("-"));
}

#[test]
fn sysroot_prefixes_are_recorded_not_resolved() {
    let options = link(&[
        "--sysroot=/sys",
        "-L=/usr/lib",
        "-L$SYSROOT/lib64",
        "-rpath-link",
        "=/opt/lib",
        "-T=/scripts/x.ld",
        "a.o",
    ]);
    assert_eq!(options.sysroot, Some(PathBuf::from("/sys")));
    assert_eq!(
        options.search_paths,
        [PathBuf::from("=/usr/lib"), PathBuf::from("$SYSROOT/lib64")]
    );
    assert_eq!(options.rpath_links, [PathBuf::from("=/opt/lib")]);
    assert_eq!(
        options.inputs[0].kind,
        InputKind::Script("=/scripts/x.ld".into())
    );
    assert_eq!(
        options.resolve_sysroot(&options.search_paths[0]),
        PathBuf::from("/sys/usr/lib")
    );
    assert_eq!(
        options.resolve_sysroot(&options.search_paths[1]),
        PathBuf::from("/sys/lib64")
    );
}

#[test]
fn emulation_selects_target() {
    let options = link(&["-m", "elf_x86_64", "a.o"]);
    assert_eq!(options.target, Some(Target::X86_64_LINUX));
    let options = link(&["-maarch64linux", "a.o"]);
    assert_eq!(options.target, Some(Target::AARCH64_LINUX));
    let options = link(&["-m", "i386pep", "a.o"]);
    let target = options.target.unwrap();
    assert_eq!(target.format, BinaryFormat::Pe);
    assert_eq!(target.arch, Architecture::X86_64);
    let options = link(&["-m", "elf64ppc", "-EL", "a.o"]);
    assert_eq!(options.target.unwrap().endian, Endianness::Big);
    assert_eq!(options.endian, Some(Endianness::Little));

    let message = error(&["-m", "elf_vax", "a.o"]);
    assert!(message.starts_with("unrecognised emulation mode: elf_vax"));
    assert!(message.contains("elf_x86_64"));
}

#[test]
fn value_parsing() {
    let options = link(&[
        "-Ttext",
        "400000",
        "-Tdata=0x600000",
        "--section-start",
        ".boot=0x7c00",
        "--image-base=0x10000",
        "-Ttext-segment=0x200000",
        "--defsym",
        " start = main + 4",
        "--build-id=0x0102-aBcD",
        "-O3",
        "--threads",
        "--error-limit=0",
        "--exclude-libs",
        "libfoo.a,libbar.a:ALL",
        "--icf=safe",
        "a.o",
    ]);
    assert_eq!(
        options.section_starts,
        [
            (".text".to_owned(), 0x40_0000),
            (".data".to_owned(), 0x60_0000),
            (".boot".to_owned(), 0x7c00),
        ]
    );
    assert_eq!(options.image_base, Some(0x10000));
    assert_eq!(options.text_segment, Some(0x20_0000));
    assert_eq!(
        options.defsym,
        [("start".to_owned(), "main + 4".to_owned())]
    );
    assert_eq!(options.build_id, BuildId::Hex(vec![0x01, 0x02, 0xab, 0xcd]));
    assert_eq!(options.optimize, 3);
    assert_eq!(options.threads, None);
    assert_eq!(options.error_limit, Some(0));
    assert_eq!(options.exclude_libs, ["libfoo.a", "libbar.a", "ALL"]);
    assert_eq!(options.icf, IcfMode::Safe);

    assert_eq!(link(&["--build-id", "a.o"]).build_id, BuildId::Sha1);
    assert_eq!(link(&["--build-id=none", "a.o"]).build_id, BuildId::None);
    assert_eq!(link(&["--icf=all", "--icf=none", "a.o"]).icf, IcfMode::None);
    assert_eq!(link(&["--thread-count", "3", "a.o"]).threads, Some(3));
    assert_eq!(link(&["--no-threads", "a.o"]).threads, Some(1));
    assert!(link(&["a.o"]).fork);
    assert!(!link(&["--no-fork", "a.o"]).fork);
    assert!(link(&["--no-fork", "--fork", "a.o"]).fork);
    assert_eq!(link(&["-O999", "a.o"]).optimize, u8::MAX);
    assert_eq!(
        link(&["--color-diagnostics", "a.o"]).color,
        ColorChoice::Always
    );
    assert_eq!(
        link(&["--no-color-diagnostics", "a.o"]).color,
        ColorChoice::Never
    );
    assert_eq!(
        link(&["--hash-style=sysv", "a.o"]).hash_style,
        HashStyle::Sysv
    );
    assert_eq!(
        link(&["--compress-debug-sections=none", "a.o"]).compress_debug_sections,
        DebugCompression::None
    );
    for (spelling, expected) in [
        ("zlib", DebugCompression::Zlib),
        ("zlib-gnu", DebugCompression::ZlibGnu),
        ("zlib-gabi", DebugCompression::ZlibGabi),
        ("zstd", DebugCompression::Zstd),
    ] {
        let argument = format!("--compress-debug-sections={spelling}");
        assert_eq!(
            link(&[&argument, "a.o"]).compress_debug_sections,
            expected,
            "{spelling}"
        );
        assert_eq!(expected.name(), spelling);
    }
}

/// The options that used to be plain strings now parse into enums. Every
/// spelling GNU ld accepts must still parse, and every value it rejects must
/// still produce GNU ld's message.
#[test]
fn enum_valued_options_keep_their_spellings_and_errors() {
    // --icf
    for (value, expected) in [
        ("none", IcfMode::None),
        ("safe", IcfMode::Safe),
        ("all", IcfMode::All),
    ] {
        let argument = format!("--icf={value}");
        assert_eq!(link(&[&argument, "a.o"]).icf, expected, "{value}");
    }
    assert_eq!(
        error(&["--icf=sometimes", "a.o"]),
        "invalid value for --icf: sometimes"
    );

    // --orphan-handling
    assert_eq!(link(&["a.o"]).orphan_handling, OrphanHandling::Place);
    for (value, expected) in [
        ("place", OrphanHandling::Place),
        ("warn", OrphanHandling::Warn),
        ("error", OrphanHandling::Error),
        ("discard", OrphanHandling::Discard),
    ] {
        let argument = format!("--orphan-handling={value}");
        assert_eq!(
            link(&[&argument, "a.o"]).orphan_handling,
            expected,
            "{value}"
        );
    }
    assert_eq!(
        error(&["--orphan-handling=keep", "a.o"]),
        "invalid value for --orphan-handling: keep"
    );

    // --sort-section
    assert_eq!(link(&["a.o"]).sort_section, SortSection::None);
    assert_eq!(
        link(&["--sort-section=name", "a.o"]).sort_section,
        SortSection::Name
    );
    assert_eq!(
        link(&["--sort-section=alignment", "a.o"]).sort_section,
        SortSection::Alignment
    );
    assert_eq!(
        error(&["--sort-section=none", "a.o"]),
        "invalid value for --sort-section: none"
    );

    // --compress-debug-sections
    assert_eq!(
        link(&["a.o"]).compress_debug_sections,
        DebugCompression::None
    );
    assert_eq!(
        error(&["--compress-debug-sections=lz4", "a.o"]),
        "invalid value for --compress-debug-sections: lz4"
    );

    // -z start-stop-visibility, which is case-insensitive
    assert_eq!(link(&["a.o"]).start_stop_visibility, None);
    for (value, expected) in [
        ("default", Visibility::Default),
        ("internal", Visibility::Internal),
        ("hidden", Visibility::Hidden),
        ("protected", Visibility::Protected),
        ("HIDDEN", Visibility::Hidden),
    ] {
        let keyword = format!("start-stop-visibility={value}");
        assert_eq!(
            link(&["-z", &keyword, "a.o"]).start_stop_visibility,
            Some(expected),
            "{value}"
        );
    }
    assert_eq!(
        error(&["-z", "start-stop-visibility=private", "a.o"]),
        "invalid value for -z start-stop-visibility: private"
    );

    // --oformat: the three raw formats are variants, every other BFD name
    // is kept as written for the format driver to check.
    assert_eq!(link(&["a.o"]).output_format, None);
    for (value, expected) in [
        ("binary", OutputFormat::Binary),
        ("ihex", OutputFormat::Ihex),
        ("srec", OutputFormat::Srec),
    ] {
        let argument = format!("--oformat={value}");
        let parsed = link(&[&argument, "a.o"]).output_format;
        assert_eq!(parsed, Some(expected.clone()), "{value}");
        assert_eq!(parsed.unwrap().name(), value);
        assert!(expected.is_raw());
    }
    let bfd = link(&["--oformat=elf64-x86-64", "a.o"])
        .output_format
        .unwrap();
    assert_eq!(bfd, OutputFormat::Bfd("elf64-x86-64".to_string()));
    assert_eq!(bfd.name(), "elf64-x86-64");
    assert!(!bfd.is_raw());
}

#[test]
fn strip_and_discard_modes() {
    assert_eq!(link(&["-s", "-S", "a.o"]).strip, StripMode::All);
    assert_eq!(link(&["-S", "a.o"]).strip, StripMode::Debug);
    assert_eq!(link(&["-x", "a.o"]).discard, DiscardMode::All);
    assert_eq!(
        link(&["-x", "--discard-none", "a.o"]).discard,
        DiscardMode::None
    );
}

#[test]
fn help_and_version_stop_parsing() {
    assert!(matches!(parse(&["--help"]), Ok(ParseOutcome::Help)));
    assert!(matches!(parse(&["-help"]), Ok(ParseOutcome::Help)));
    assert!(matches!(
        parse(&["a.o", "--version"]),
        Ok(ParseOutcome::Version)
    ));
    assert!(matches!(parse(&["-V"]), Ok(ParseOutcome::Version)));
    assert!(matches!(
        parse(&["-v", "--bogus"]),
        Ok(ParseOutcome::Version)
    ));
    // An earlier error wins, as in GNU ld.
    assert!(parse(&["--bogus", "-v"]).is_err());
    // `-o -v` names an output file called "-v".
    assert_eq!(link(&["-o", "-v", "a.o"]).output, Some(PathBuf::from("-v")));
}

#[test]
fn plugins_collect_their_options() {
    let options = link(&[
        "-plugin",
        "a.so",
        "-plugin-opt=x",
        "--plugin-opt",
        "y",
        "-plugin=b.so",
        "-plugin-opt=-pass-through=-lgcc",
        "a.o",
    ]);
    assert_eq!(
        options.plugins,
        [
            (PathBuf::from("a.so"), vec!["x".to_owned(), "y".to_owned()]),
            (
                PathBuf::from("b.so"),
                vec!["-pass-through=-lgcc".to_owned()]
            ),
        ]
    );
    let options = link(&["-plugin-opt=mcpu=x86-64", "a.o"]);
    assert!(options.plugins.is_empty());
    assert_eq!(options.warnings.len(), 1);
}

#[test]
fn non_utf8_paths_survive() {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9.o");
        let output = std::ffi::OsStr::from_bytes(b"-o\xff");
        let argv = [std::ffi::OsStr::new("ld"), output, name];
        let Ok(ParseOutcome::Link(options)) = parse_gnu_with(&argv, &no_files) else {
            panic!("non-UTF-8 arguments must parse");
        };
        assert_eq!(options.inputs[0].kind, InputKind::File(PathBuf::from(name)));
        assert_eq!(
            options.output.as_deref().map(|p| p.as_os_str().as_bytes()),
            Some(&b"\xff"[..])
        );
        let soname = [
            std::ffi::OsStr::new("ld"),
            std::ffi::OsStr::from_bytes(b"-soname=\xff"),
            name,
        ];
        assert!(matches!(
            parse_gnu_with(&soname, &no_files),
            Err(Error::Option(message)) if message.contains("UTF-8")
        ));
    }
}

// ---------------------------------------------------------------------------
// Positional state
// ---------------------------------------------------------------------------

#[test]
fn whole_archive_and_as_needed_apply_to_following_inputs() {
    let options = link(&[
        "a.o",
        "--whole-archive",
        "libw.a",
        "--no-whole-archive",
        "--as-needed",
        "-lm",
        "--no-as-needed",
        "-lc",
    ]);
    assert!(!attrs_of(&options, &file("a.o")).whole_archive);
    assert!(attrs_of(&options, &file("libw.a")).whole_archive);
    assert!(!attrs_of(&options, &file("libw.a")).as_needed);
    assert!(attrs_of(&options, &lib("m")).as_needed);
    assert!(!attrs_of(&options, &lib("c")).as_needed);
    let positions: Vec<usize> = options.inputs.iter().map(|i| i.position).collect();
    assert_eq!(positions, [0, 1, 2, 3]);
}

#[test]
fn static_aliases() {
    for (on, off) in [
        ("-Bstatic", "-Bdynamic"),
        ("-static", "-dy"),
        ("-dn", "-call_shared"),
        ("-non_shared", "--Bdynamic"),
        ("--static", "-Bdynamic"),
    ] {
        let options = link(&[on, "-la", off, "-lb"]);
        assert!(attrs_of(&options, &lib("a")).static_only, "{on}");
        assert!(!attrs_of(&options, &lib("b")).static_only, "{off}");
    }
    let options = link(&["-a", "archive", "-la", "-a", "default", "-lb"]);
    assert!(attrs_of(&options, &lib("a")).static_only);
    assert!(!attrs_of(&options, &lib("b")).static_only);
}

#[test]
fn push_and_pop_state() {
    let options = link(&[
        "--whole-archive",
        "-Bstatic",
        "--push-state",
        "--no-whole-archive",
        "--as-needed",
        "-Bdynamic",
        "--copy-dt-needed-entries",
        "-b",
        "binary",
        "--start-group",
        "-lx",
        "--pop-state",
        "-ly",
        "--end-group",
        "-lz",
    ]);
    let x = attrs_of(&options, &lib("x"));
    assert!(!x.whole_archive && x.as_needed && !x.static_only && x.copy_dt_needed);
    assert_eq!(x.format, InputFormat::Binary);
    assert!(x.in_group);
    let y = attrs_of(&options, &lib("y"));
    assert!(y.whole_archive && !y.as_needed && y.static_only && !y.copy_dt_needed);
    assert_eq!(y.format, InputFormat::Auto);
    assert!(y.in_group, "--pop-state does not end a group");
    assert!(!attrs_of(&options, &lib("z")).in_group);

    assert!(error(&["--pop-state", "a.o"]).contains("--push-state"));
}

#[test]
fn groups_and_libs() {
    let options = link(&["-(", "-la", "-)", "--start-lib", "x.o", "--end-lib", "y.o"]);
    assert!(attrs_of(&options, &lib("a")).in_group);
    assert!(attrs_of(&options, &file("x.o")).lazy);
    assert!(!attrs_of(&options, &file("y.o")).lazy);
    assert!(error(&["--start-group", "--start-group", "a.o"]).contains("nested"));
    assert!(error(&["a.o", "--end-group"]).contains("--start-group"));
    assert!(error(&["a.o", "--end-lib"]).contains("--start-lib"));
    // An unterminated group is accepted, as in lld.
    assert!(attrs_of(&link(&["--start-group", "a.o"]), &file("a.o")).in_group);
}

#[test]
fn format_applies_to_following_inputs() {
    let options = link(&[
        "-b",
        "binary",
        "blob.bin",
        "--format=default",
        "a.o",
        "--format",
        "elf64-x86-64",
        "b.o",
    ]);
    assert_eq!(
        attrs_of(&options, &file("blob.bin")).format,
        InputFormat::Binary
    );
    assert_eq!(attrs_of(&options, &file("a.o")).format, InputFormat::Auto);
    assert_eq!(attrs_of(&options, &file("b.o")).format, InputFormat::Auto);
}

#[test]
fn scripts_and_just_symbols_are_positional_inputs() {
    let options = link(&[
        "a.o",
        "-T",
        "link.ld",
        "--just-symbols=syms.o",
        "-dT",
        "default.ld",
    ]);
    let kinds: Vec<_> = options.inputs.iter().map(|i| i.kind.clone()).collect();
    assert_eq!(
        kinds,
        [
            file("a.o"),
            InputKind::Script("link.ld".into()),
            InputKind::JustSymbols("syms.o".into()),
        ]
    );
    assert_eq!(options.default_script, Some(PathBuf::from("default.ld")));
}

#[test]
fn output_kind() {
    assert_eq!(link(&["a.o"]).kind, OutputKind::Executable);
    assert_eq!(link(&["-pie", "a.o"]).kind, OutputKind::Pie);
    assert_eq!(
        link(&["-pie", "-no-pie", "a.o"]).kind,
        OutputKind::Executable
    );
    assert_eq!(link(&["-shared", "a.o"]).kind, OutputKind::Shared);
    assert_eq!(link(&["-pie", "-shared", "a.o"]).kind, OutputKind::Shared);
    assert_eq!(link(&["-shared", "-pie", "a.o"]).kind, OutputKind::Pie);
    assert_eq!(link(&["-r", "a.o"]).kind, OutputKind::Relocatable);
    assert_eq!(link(&["-static", "a.o"]).kind, OutputKind::StaticExecutable);
    assert_eq!(
        link(&["-static", "-pie", "a.o"]).kind,
        OutputKind::StaticPie
    );
    assert_eq!(
        link(&["-pie", "--no-dynamic-linker", "a.o"]).kind,
        OutputKind::StaticPie
    );
    // Only the state at the end of the command line decides: rustc brackets
    // its static libraries with -Bstatic ... -Bdynamic.
    assert_eq!(
        link(&["-pie", "-Bstatic", "-lstd", "-Bdynamic", "-lc", "a.o"]).kind,
        OutputKind::Pie
    );
    assert_eq!(
        link(&["-static", "--push-state", "-Bdynamic", "--pop-state", "a.o"]).kind,
        OutputKind::StaticExecutable
    );
    assert!(error(&["-r", "-shared", "a.o"]).contains("-shared"));
    assert!(error(&["-r", "-pie", "a.o"]).contains("-pie"));
}

// ---------------------------------------------------------------------------
// Response files
// ---------------------------------------------------------------------------

fn files(
    entries: &'static [(&'static str, &'static str)],
) -> impl Fn(&Path) -> io::Result<Vec<u8>> {
    move |path: &Path| {
        entries
            .iter()
            .find(|(name, _)| Path::new(name) == path)
            .map(|(_, contents)| contents.as_bytes().to_vec())
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

fn link_with(args: &[&str], reader: &dyn qld::args::FileReader) -> Box<LinkOptions> {
    let mut argv = vec!["ld"];
    argv.extend_from_slice(args);
    match parse_gnu_with(&argv, reader) {
        Ok(ParseOutcome::Link(options)) => options,
        other => panic!("{args:?}: {other:?}"),
    }
}

#[test]
fn response_files_expand_recursively_with_gnu_quoting() {
    let reader = files(&[
        ("outer.rsp", "-o 'my output' @inner.rsp \"a b.o\"\n"),
        ("inner.rsp", "-L\\ dir\\ with\\ spaces -lc 'it'\\''s.o'"),
    ]);
    let options = link_with(&["--rsp-quoting=posix", "@outer.rsp", "last.o"], &reader);
    assert_eq!(options.output, Some(PathBuf::from("my output")));
    assert_eq!(options.search_paths, [PathBuf::from(" dir with spaces")]);
    let kinds: Vec<_> = options.inputs.iter().map(|i| i.kind.clone()).collect();
    assert_eq!(
        kinds,
        [lib("c"), file("it's.o"), file("a b.o"), file("last.o")]
    );
}

#[test]
fn response_file_errors() {
    let reader = files(&[("self.rsp", "@self.rsp")]);
    let mut argv = vec!["ld", "@self.rsp"];
    assert!(matches!(
        parse_gnu_with(&argv, &reader),
        Err(Error::Option(message)) if message.contains("nested too deeply")
    ));
    argv[1] = "@missing.rsp";
    assert!(matches!(
        parse_gnu_with(&argv, &reader),
        Err(Error::Io { path: Some(path), .. }) if path == Path::new("missing.rsp")
    ));
}

#[test]
fn response_file_can_supply_option_values() {
    let reader = files(&[("value.rsp", "libfoo.so.1")]);
    let options = link_with(&["-soname", "@value.rsp", "a.o"], &reader);
    assert_eq!(options.soname.as_deref(), Some("libfoo.so.1"));
}

#[test]
fn windows_response_file_quoting() {
    let reader = files(&[(
        "win.rsp",
        r#"C:\obj\a.o "C:\Program Files\b.o" -o out\x.exe"#,
    )]);
    let options = link_with(&["--rsp-quoting=windows", "@win.rsp"], &reader);
    let kinds: Vec<_> = options.inputs.iter().map(|i| i.kind.clone()).collect();
    assert_eq!(kinds, [file(r"C:\obj\a.o"), file(r"C:\Program Files\b.o")]);
    assert_eq!(options.output, Some(PathBuf::from(r"out\x.exe")));

    let options = link_with(&["--rsp-quoting", "posix", "@win.rsp"], &reader);
    assert_eq!(options.inputs[0].kind, file("C:obja.o"));
}

// ---------------------------------------------------------------------------
// Flavors
// ---------------------------------------------------------------------------

#[test]
fn flavor_from_argv0_and_flag() {
    let gnu = |argv: &[&str]| select_flavor(argv).unwrap();
    assert_eq!(gnu(&["qld"]), (Flavor::Gnu, 1));
    assert_eq!(gnu(&["/usr/bin/ld"]), (Flavor::Gnu, 1));
    assert_eq!(gnu(&["ld.qld"]), (Flavor::Gnu, 1));
    assert_eq!(gnu(&["x86_64-linux-gnu-ld"]), (Flavor::Gnu, 1));
    assert_eq!(gnu(&["/opt/bin/ld64.qld"]), (Flavor::Darwin, 1));
    assert_eq!(gnu(&["LD64.EXE"]), (Flavor::Darwin, 1));
    assert_eq!(gnu(&["ld64", "-flavor", "gnu", "a.o"]), (Flavor::Gnu, 3));
    assert_eq!(gnu(&["qld", "-flavor", "darwin"]), (Flavor::Darwin, 3));
    assert_eq!(gnu(&[]), (Flavor::Gnu, 1));

    assert!(matches!(
        select_flavor(&["lld-link"]),
        Err(Error::Unimplemented(_))
    ));
    assert!(matches!(
        select_flavor(&["qld", "-flavor", "link"]),
        Err(Error::Unimplemented(_))
    ));
    assert!(matches!(
        select_flavor(&["qld", "-flavor", "vax"]),
        Err(Error::Option(_))
    ));
    assert!(matches!(
        select_flavor(&["qld", "-flavor"]),
        Err(Error::Option(_))
    ));
    // -flavor is only special as the first argument; elsewhere it is
    // `-f lavor`, as in GNU ld.
    assert_eq!(link(&["a.o", "-flavor", "gnu"]).auxiliary, ["lavor"]);
}

#[test]
fn flavor_dispatch() {
    let options = match parse_gnu_with(&["qld", "-flavor", "gnu", "-o", "x", "a.o"], &no_files) {
        Ok(ParseOutcome::Link(options)) => options,
        other => panic!("{other:?}"),
    };
    assert_eq!(options.output, Some(PathBuf::from("x")));
    match parse_gnu_with(&["ld64.qld", "-arch", "arm64", "a.o"], &no_files) {
        Ok(ParseOutcome::Link(options)) => assert_eq!(options.flavor, Flavor::Darwin),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        parse_gnu_with(&["ld64.qld", "-v"], &no_files),
        Ok(ParseOutcome::Version)
    ));
}

// ---------------------------------------------------------------------------
// Real command lines
// ---------------------------------------------------------------------------

fn corpus(name: &'static str, contents: &'static str) -> Box<LinkOptions> {
    let reader = move |path: &Path| {
        if path == Path::new(name) {
            Ok(contents.as_bytes().to_vec())
        } else {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }
    };
    let arg = format!("@{name}");
    link_with(&["--rsp-quoting=posix", &arg], &reader)
}

const GCC_CRT: &str = "/usr/lib/gcc/x86_64-pc-linux-gnu/15";

#[test]
fn gcc_dynamic_pie() {
    let o = corpus(
        "gcc-dynamic-pie.rsp",
        include_str!("data/args/gcc-dynamic-pie.rsp"),
    );
    assert_eq!(o.kind, OutputKind::Pie);
    assert_eq!(o.target, Some(Target::X86_64_LINUX));
    assert_eq!(o.output, Some(PathBuf::from("hello")));
    assert_eq!(
        o.dynamic_linker,
        Some(PathBuf::from("/lib64/ld-linux-x86-64.so.2"))
    );
    assert_eq!(o.build_id, BuildId::Sha1);
    assert!(o.eh_frame_hdr && o.bind_now);
    assert_eq!(o.search_paths.len(), 8);
    assert_eq!(o.plugins.len(), 1);
    assert_eq!(o.plugins[0].1.len(), 7);
    assert!(o.warnings.is_empty() && o.ignored.is_empty());
    let names: Vec<_> = o.inputs.iter().map(|i| i.kind.clone()).collect();
    assert_eq!(names[3], file("h.o"));
    assert_eq!(names[4], lib("gcc"));
    assert_eq!(names[5], lib("gcc_s"));
    assert!(attrs_of(&o, &lib("gcc_s")).as_needed);
    assert!(!attrs_of(&o, &lib("c")).as_needed);
    assert_eq!(o.inputs.len(), 11);
    assert_eq!(
        o.inputs.last().unwrap().kind,
        file(&format!("{GCC_CRT}/../../../../lib64/crtn.o"))
    );
}

#[test]
fn gcc_static() {
    let o = corpus("gcc-static.rsp", include_str!("data/args/gcc-static.rsp"));
    assert_eq!(o.kind, OutputKind::StaticExecutable);
    assert_eq!(o.dynamic_linker, None);
    assert!(!o.eh_frame_hdr);
    let group: Vec<_> = o
        .inputs
        .iter()
        .filter(|i| i.attrs.in_group)
        .map(|i| i.kind.clone())
        .collect();
    assert_eq!(group, [lib("gcc"), lib("gcc_eh"), lib("c")]);
    assert!(o.inputs.iter().all(|i| i.attrs.static_only));
}

#[test]
fn gcc_shared() {
    let o = corpus("gcc-shared.rsp", include_str!("data/args/gcc-shared.rsp"));
    assert_eq!(o.kind, OutputKind::Shared);
    assert_eq!(o.output, Some(PathBuf::from("libf.so")));
    assert!(o.is_pic() && o.is_dynamic());
    assert!(o.inputs.iter().any(|i| i.kind == file("fpic.o")));
}

#[test]
fn gcc_static_pie() {
    let o = corpus(
        "gcc-static-pie.rsp",
        include_str!("data/args/gcc-static-pie.rsp"),
    );
    assert_eq!(o.kind, OutputKind::StaticPie);
    assert!(o.no_dynamic_linker);
    assert!(o.error_textrel);
}

#[test]
fn clang_pie() {
    let o = corpus("clang-pie.rsp", include_str!("data/args/clang-pie.rsp"));
    assert_eq!(o.kind, OutputKind::Pie);
    assert_eq!(o.hash_style, HashStyle::Gnu);
    assert!(o.relro && o.bind_now && o.eh_frame_hdr);
    assert!(o.plugins.is_empty());
    let as_needed: Vec<_> = o
        .inputs
        .iter()
        .filter(|i| i.attrs.as_needed)
        .map(|i| i.kind.clone())
        .collect();
    assert_eq!(as_needed, [lib("gcc_s"), lib("gcc_s")]);
}

#[test]
fn rustc_pie() {
    let o = corpus("rustc-pie.rsp", include_str!("data/args/rustc-pie.rsp"));
    assert_eq!(o.kind, OutputKind::Pie);
    assert!(o.gc_sections);
    assert_eq!(o.exec_stack, ExecStack::NonExecutable);
    assert_eq!(o.ignored, [OsString::from("-fuse-ld=lld")]);
    let rlibs: Vec<_> = o
        .inputs
        .iter()
        .filter(
            |i| matches!(&i.kind, InputKind::File(p) if p.extension().is_some_and(|e| e == "rlib")),
        )
        .collect();
    assert_eq!(rlibs.len(), 19);
    assert!(
        rlibs
            .iter()
            .all(|i| i.attrs.static_only && i.attrs.as_needed)
    );
    let c = attrs_of(&o, &lib("c"));
    assert!(!c.static_only && c.as_needed);
}

#[test]
fn linux_kernel() {
    let o = corpus(
        "linux-vmlinux.rsp",
        include_str!("data/args/linux-vmlinux.rsp"),
    );
    assert_eq!(o.kind, OutputKind::Executable);
    assert!(o.emit_relocs);
    assert_eq!(o.discard, DiscardMode::Locals);
    assert_eq!(o.strip, StripMode::Debug);
    assert_eq!(o.max_page_size, Some(0x20_0000));
    assert_eq!(o.orphan_handling, OrphanHandling::Warn);
    assert_eq!(
        o.inputs[0].kind,
        InputKind::Script("./arch/x86/kernel/vmlinux.lds".into())
    );
    assert!(attrs_of(&o, &file("vmlinux.a")).whole_archive);
    assert!(!attrs_of(&o, &file(".tmp_vmlinux.kallsyms2.o")).whole_archive);
    assert_eq!(o.ignored, [OsString::from("--no-warn-rwx-segments")]);
}

#[test]
fn clang_lld_android_shared_library() {
    let o = corpus(
        "clang-lld-android-shared.rsp",
        include_str!("data/args/clang-lld-android-shared.rsp"),
    );
    assert_eq!(o.kind, OutputKind::Shared);
    assert_eq!(o.target, Some(Target::AARCH64_LINUX));
    assert_eq!(o.endian, Some(Endianness::Little));
    assert!(o.fix_cortex_a53_843419);
    assert_eq!(o.max_page_size, Some(16384));
    assert_eq!(o.rosegment, Some(false));
    assert_eq!(o.undefined_version, Some(false));
    assert!(o.fatal_warnings);
    assert_eq!(o.no_undefined, Some(true));
    assert_eq!(o.soname.as_deref(), Some("libnative.so"));
    assert_eq!(o.icf, IcfMode::Safe);
    assert_eq!(o.exclude_libs, ["libunwind.a", "libgcc.a"]);
    assert_eq!(
        o.search_paths,
        [
            PathBuf::from("=/usr/lib/aarch64-linux-android/24"),
            PathBuf::from("$SYSROOT/usr/lib")
        ]
    );
    assert!(
        o.inputs
            .iter()
            .any(|i| i.kind == file("dir with spaces/helper.o"))
    );
    assert!(attrs_of(&o, &lib("c++_static")).static_only);
    assert!(!attrs_of(&o, &InputKind::LibraryExact("libc++abi.a".into())).static_only);
    assert!(attrs_of(&o, &lib("m")).as_needed);
    assert!(!attrs_of(&o, &lib("c")).as_needed);
    assert!(attrs_of(&o, &file("lazy1.o")).lazy);
    assert!(!attrs_of(&o, &lib("dl")).lazy);
    assert_eq!(
        o.ignored,
        [
            OsString::from("-plugin-opt=mcpu=cortex-a53"),
            OsString::from("--thinlto-cache-dir=/tmp/thinlto")
        ]
    );
    assert_eq!(o.warnings.len(), 1);
}

#[test]
fn mold_rust_release() {
    let o = corpus(
        "mold-rust-release.rsp",
        include_str!("data/args/mold-rust-release.rsp"),
    );
    assert_eq!(o.kind, OutputKind::Pie);
    assert_eq!(o.color, ColorChoice::Always);
    assert_eq!(o.threads, Some(8));
    assert_eq!(o.hash_style, HashStyle::Both);
    assert_eq!(o.build_id, BuildId::Hex(vec![0xde, 0xad, 0xbe, 0xef]));
    assert!(o.gc_sections && o.print_gc_sections && o.pack_relative_relocs);
    assert_eq!(o.icf, IcfMode::All);
    assert_eq!(o.strip, StripMode::All);
    assert_eq!(o.separate_code, Some(SeparateCode::Loadable));
    assert_eq!(o.x86.isa_level, 3);
    assert!(o.x86.ibt && o.x86.shstk);
    assert_eq!(o.x86.cet_report, qld::args::ReportLevel::Warning);
    assert_eq!(o.exec_stack, ExecStack::FromInputs);
    assert_eq!(o.optimize, 2);
    assert_eq!(
        o.defsym,
        [("__rust_probestack".to_owned(), "__probestack".to_owned())]
    );
    assert_eq!(o.wrap, ["malloc"]);
    assert_eq!(o.undefined, ["__rust_alloc"]);
    assert_eq!(o.export_dynamic_symbols, ["plugin_*"]);
    assert_eq!(o.version_scripts, [PathBuf::from("exports.map")]);
    assert_eq!(o.symbolic, SymbolicMode::Functions);
    assert_eq!(o.map_file, Some(PathBuf::from("out.map")));
    assert_eq!(o.why_live, ["main"]);
    assert_eq!(o.rpaths, [PathBuf::from("$ORIGIN/../lib")]);
    assert_eq!(o.new_dtags, Some(true));
    assert_eq!(o.inputs.last().unwrap().kind, file("-weird-name.o"));
    assert_eq!(o.ignored, [OsString::from("--quick-exit")]);
    assert!(!o.fork);
}

// ---------------------------------------------------------------------------
// PE/COFF (MinGW) options
// ---------------------------------------------------------------------------

/// The link line `x86_64-w64-mingw32-gcc` passes for a console executable.
#[test]
fn mingw_console_command_line() {
    let o = link(&[
        "--sysroot=/usr/lib/mingw64-toolchain",
        "-m",
        "i386pep",
        "-Bdynamic",
        "-o",
        "t.exe",
        "crt2.o",
        "-L/usr/lib/mingw64-toolchain/mingw/lib",
        "hello.o",
        "-lmingw32",
        "-lgcc",
        "-lmsvcrt",
        "-lkernel32",
    ]);
    assert_eq!(o.target.map(|target| target.format), Some(BinaryFormat::Pe));
    assert_eq!(o.kind, OutputKind::Executable);
    assert_eq!(o.output, Some(PathBuf::from("t.exe")));
    assert!(o.warnings.is_empty() && o.ignored.is_empty());
    // Nothing on that line touches the PE options, so they are all defaults.
    assert_eq!(o.pe, qld::args::PeArgs::default());
}

/// `-mwindows` adds `--subsystem windows`, and `-shared` adds `--shared`,
/// `--enable-auto-image-base` and `--out-implib`.
#[test]
fn mingw_windows_and_shared_command_lines() {
    let o = link(&[
        "-m",
        "i386pep",
        "--subsystem",
        "windows",
        "-Bdynamic",
        "a.o",
    ]);
    assert_eq!(o.pe.subsystem, Some(2)); // IMAGE_SUBSYSTEM_WINDOWS_GUI
    assert!(o.warnings.is_empty() && o.ignored.is_empty());

    let o = link(&[
        "-m",
        "i386pep",
        "--shared",
        "-Bdynamic",
        "-e",
        "DllMainCRTStartup",
        "--enable-auto-image-base",
        "-o",
        "d.dll",
        "d.o",
        "--out-implib",
        "libd.dll.a",
    ]);
    assert_eq!(o.kind, OutputKind::Shared);
    assert_eq!(o.entry.as_deref(), Some("DllMainCRTStartup"));
    assert_eq!(o.pe.out_implib, Some(PathBuf::from("libd.dll.a")));
    assert_eq!(o.ignored, [OsString::from("--enable-auto-image-base")]);
    assert!(o.warnings.is_empty());

    let pe = qld::coff::PeOptions::from_link_options(&o);
    assert!(pe.dll);
    assert_eq!(pe.out_implib, Some(PathBuf::from("libd.dll.a")));
}

/// `--dll` is GNU ld's PE spelling of `-shared`.
#[test]
fn dll_is_shared() {
    for flag in ["-shared", "--shared", "--dll", "-dll"] {
        let o = link(&[flag, "a.o"]);
        assert_eq!(o.kind, OutputKind::Shared, "{flag}");
        assert!(qld::coff::PeOptions::from_link_options(&o).dll, "{flag}");
    }
    assert!(!qld::coff::PeOptions::from_link_options(&link(&["a.o"])).dll);
}

/// The value formats of the PE options that take one.
#[test]
fn pe_option_values() {
    let o = link(&[
        "--subsystem",
        "windows,6.1",
        "--stack",
        "0x100000,0x1000",
        "--heap",
        "2097152",
        "--image-base=0x1c0000000",
        "--section-alignment",
        "0x2000",
        "--file-alignment=0x400",
        "--major-image-version",
        "2",
        "--minor-image-version=11",
        "--major-os-version",
        "6",
        "--minor-os-version",
        "1",
        "--major-subsystem-version=6",
        "--minor-subsystem-version",
        "2",
        "a.o",
    ]);
    assert_eq!(o.pe.subsystem, Some(2));
    assert_eq!(o.pe.major_subsystem_version, 6);
    assert_eq!(o.pe.minor_subsystem_version, 2);
    assert_eq!(o.pe.stack, (0x10_0000, 0x1000));
    // Only a reserve: the commit size keeps its default.
    assert_eq!(o.pe.heap, (0x20_0000, 0x1000));
    assert_eq!(o.image_base, Some(0x1_c000_0000));
    assert_eq!(o.pe.section_alignment, 0x2000);
    assert_eq!(o.pe.file_alignment, 0x400);
    assert_eq!(
        (o.pe.major_image_version, o.pe.minor_image_version),
        (2, 11)
    );
    assert_eq!((o.pe.major_os_version, o.pe.minor_os_version), (6, 1));

    // `--subsystem NAME,MAJOR.MINOR` also sets the subsystem version, and
    // `--subsystem N` takes a raw number.
    let o = link(&["--subsystem", "console,6.1", "a.o"]);
    assert_eq!(o.pe.subsystem, Some(3));
    assert_eq!(
        (o.pe.major_subsystem_version, o.pe.minor_subsystem_version),
        (6, 1)
    );
    assert_eq!(link(&["--subsystem=native", "a.o"]).pe.subsystem, Some(1));
    assert_eq!(link(&["--subsystem=10", "a.o"]).pe.subsystem, Some(10));
    assert_eq!(
        link(&["--subsystem", "efi-app", "a.o"]).pe.subsystem,
        Some(10)
    );

    // `--stack` and `-z stack-size` set the same field; the last one wins.
    let o = link(&["-z", "stack-size=0x40000", "a.o"]);
    assert_eq!(o.pe.stack.0, 0x4_0000);
    assert_eq!(o.stack_size, Some(0x4_0000));
    assert_eq!(
        link(&["-z", "stack-size=0x40000", "--stack", "0x80000", "a.o"])
            .pe
            .stack
            .0,
        0x8_0000
    );

    assert!(error(&["--subsystem", "bogus", "a.o"]).contains("bogus"));
    assert!(error(&["--stack", "x", "a.o"]).contains("--stack"));
    assert!(error(&["--stack", "0x1000,x", "a.o"]).contains("--stack"));
    assert!(error(&["--major-os-version", "70000", "a.o"]).contains("--major-os-version"));
    assert!(error(&["--section-alignment", "0x100000000", "a.o"]).contains("--section-alignment"));
}

/// The image flags, their `--disable-*` forms, and the lists.
#[test]
fn pe_flags_and_lists() {
    let defaults = qld::args::PeArgs::default();
    assert!(defaults.dynamicbase && defaults.nxcompat && defaults.high_entropy_va);
    assert!(defaults.large_address_aware && defaults.reloc_section);
    assert!(defaults.auto_import && defaults.runtime_pseudo_reloc);
    assert!(!defaults.insert_timestamp && !defaults.tsaware);

    let o = link(&[
        "--disable-dynamicbase",
        "--disable-nxcompat",
        "--disable-high-entropy-va",
        "--disable-large-address-aware",
        "--disable-reloc-section",
        "--disable-auto-import",
        "--disable-runtime-pseudo-reloc",
        "--tsaware",
        "--no-seh",
        "--forceinteg",
        "--no-isolation",
        "--no-bind",
        "--wdmdriver",
        "--insert-timestamp",
        "--kill-at",
        "--add-stdcall-alias",
        "--enable-stdcall-fixup",
        "--export-all-symbols",
        "--exclude-all-symbols",
        "--warn-duplicate-exports",
        "--exclude-symbols",
        "secret,_hidden@4",
        "--exclude-modules-for-implib=libfoo.a,bar.o",
        "--export=add_one",
        "--export",
        "alias=real,@7,NONAME",
        "--out-implib",
        "libx.dll.a",
        "--output-def=x.def",
        "a.o",
    ]);
    let pe = &o.pe;
    assert!(!pe.dynamicbase && !pe.nxcompat && !pe.high_entropy_va);
    assert!(!pe.large_address_aware && !pe.reloc_section);
    assert!(!pe.auto_import && !pe.runtime_pseudo_reloc);
    assert!(pe.tsaware && pe.no_seh && pe.forceinteg && pe.no_isolation);
    assert!(pe.no_bind && pe.wdmdriver && pe.insert_timestamp);
    assert!(pe.kill_at && pe.add_stdcall_alias && pe.export_all_symbols);
    assert!(pe.exclude_all_symbols && pe.warn_duplicate_exports);
    assert_eq!(pe.stdcall_fixup, Some(true));
    assert_eq!(pe.exclude_symbols, ["secret", "_hidden@4"]);
    assert_eq!(pe.exclude_modules_for_implib, ["libfoo.a", "bar.o"]);
    assert_eq!(pe.exports, ["add_one", "alias=real,@7,NONAME"]);
    assert_eq!(pe.out_implib, Some(PathBuf::from("libx.dll.a")));
    assert_eq!(pe.output_def, Some(PathBuf::from("x.def")));

    // Everything reaches the PE backend's options.
    let backend = qld::coff::PeOptions::from_link_options(&o);
    assert!(!backend.dynamicbase && !backend.nxcompat && !backend.high_entropy_va);
    assert!(backend.disable_reloc_section && backend.insert_timestamp);
    assert_eq!(
        backend.auto_import,
        qld::coff::options::AutoImport::Disabled
    );
    assert!(!backend.runtime_pseudo_reloc);
    assert_eq!(backend.enable_stdcall_fixup, Some(true));
    assert_eq!(
        backend.exclude_symbols,
        [b"secret".to_vec(), b"_hidden@4".to_vec()]
    );
    assert_eq!(backend.exports.len(), 2);
    assert_eq!(backend.output_def, Some(PathBuf::from("x.def")));

    // The `--no-*` spellings of the flags GNU ld also accepts that way.
    assert!(!link(&["--no-dynamicbase", "a.o"]).pe.dynamicbase);
    assert_eq!(
        link(&["--disable-stdcall-fixup", "a.o"]).pe.stdcall_fixup,
        Some(false)
    );
    assert!(!link(&["--no-insert-timestamp", "a.o"]).pe.insert_timestamp);
}

/// A bare `.def` file is a module-definition file, not an object.
#[test]
fn def_file_is_a_positional_input() {
    let o = link(&["a.o", "exports.def"]);
    assert_eq!(o.pe.def_file, Some(PathBuf::from("exports.def")));
    let kinds: Vec<_> = o.inputs.iter().map(|i| i.kind.clone()).collect();
    assert_eq!(kinds, [file("a.o")]);
    assert_eq!(
        qld::coff::PeOptions::from_link_options(&o).def_file,
        Some(PathBuf::from("exports.def"))
    );
    // The extension match is case-insensitive, and works after `--` too.
    assert_eq!(
        link(&["a.o", "--", "EXPORTS.DEF"]).pe.def_file,
        Some(PathBuf::from("EXPORTS.DEF"))
    );
    // A name that only contains `.def` is an ordinary input.
    assert_eq!(link(&["a.def.o"]).inputs[0].kind, file("a.def.o"));
    assert!(link(&["a.o"]).pe.def_file.is_none());
    assert!(error(&["a.o", "one.def", "two.def"]).contains("only one .def file"));
}

/// PE options parse whatever the target is, and change nothing about an ELF
/// link.
#[test]
fn pe_options_do_not_disturb_an_elf_link() {
    let elf = link(&["-m", "elf_x86_64", "a.o"]);
    let with_pe = link(&[
        "-m",
        "elf_x86_64",
        "--subsystem",
        "windows",
        "--dynamicbase",
        "--major-image-version",
        "3",
        "--out-implib",
        "x.a",
        "a.o",
    ]);
    let strip_pe = |options: &LinkOptions| {
        let mut options = options.clone();
        options.pe = qld::args::PeArgs::default();
        format!("{options:?}")
    };
    assert_eq!(strip_pe(&elf), strip_pe(&with_pe));
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

/// A small deterministic generator, so failures reproduce.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(self.0 >> 33).unwrap()
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.next() % items.len()]
    }
}

#[test]
fn random_command_lines_never_panic() {
    let mut fragments: Vec<&str> = vec![
        "-", "--", "-z", "=", "@", "@r", "@", "a.o", "", "'", "\"", "\\", "-(", "-)", "-l:", "=x",
        "0x", "$SYSROOT", "-flavor", "gnu", "darwin",
    ];
    fragments.extend(GNU_OPTIONS.iter().map(|def| def.name));
    fragments.extend(Z_KEYWORDS.iter().map(|z| z.name));
    let reader = |path: &Path| -> io::Result<Vec<u8>> {
        match path.to_str() {
            Some("r") => Ok(b"-o 'x y' @r2 \"unterminated".to_vec()),
            Some("r2") => Ok(b"--start-group -lc @r".to_vec()),
            _ => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    };
    let mut rng = Lcg(0x5eed);
    for _ in 0..20_000 {
        let count = rng.next() % 8;
        let mut argv = vec!["ld".to_owned()];
        for _ in 0..count {
            let mut arg = String::new();
            match rng.next() % 4 {
                0 => arg.push('-'),
                1 => arg.push_str("--"),
                _ => {}
            }
            for _ in 0..=rng.next() % 3 {
                arg.push_str(rng.pick(&fragments));
            }
            argv.push(arg);
        }
        let _ = parse_gnu_with(&argv, &reader);
    }
}

#[test]
fn dead_reloc_in_nonalloc_rules_accumulate() {
    let options = link(&[
        "-z",
        "dead-reloc-in-nonalloc=.debug_*=0",
        "-zdead-reloc-in-nonalloc=.debug_ranges=0x1",
        "in.o",
    ]);
    assert_eq!(
        options.dead_reloc_in_nonalloc,
        [(".debug_*".to_owned(), 0), (".debug_ranges".to_owned(), 1)]
    );
    assert!(
        error(&["-z", "dead-reloc-in-nonalloc=.debug_info=zz", "in.o"])
            .contains("dead-reloc-in-nonalloc")
    );
}

#[test]
fn help_names_supported_targets_for_libtool() {
    // libtool: `$LD --help 2>&1 | $EGREP ': supported targets:.* elf'`.
    let help = qld::args::usage();
    let line = help
        .lines()
        .find(|l| l.contains(": supported targets:"))
        .expect("supported targets line");
    assert!(line.contains(" elf"), "{line}");
    assert!(help.contains(": supported emulations: elf_x86_64"));
    // The MinGW emulation qld links too.
    assert!(line.contains("pei-x86-64"), "{line}");
    assert!(help.contains("i386pep"));
}
