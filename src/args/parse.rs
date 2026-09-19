//! argv front ends.
//!
//! [`parse_gnu`] selects the flavor from `argv[0]` or a leading `-flavor`,
//! expands `@response` files and then interprets the GNU ld / gold / lld /
//! mold command line using the option table in [`crate::args::table`].
//!
//! The rules — single- versus double-dash long options, the `-o` exception,
//! joined and separate values, `-z` keywords, response files, `=`/`$SYSROOT`
//! prefixes, positional state, and the implemented/ignored/unsupported
//! policy — are specified in `docs/compatibility.md`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::args::emulation;
use crate::args::options::{
    BuildId, ColorChoice, DebugCompression, HashStyle, IcfMode, InputAttrs, InputFormat, InputKind,
    LinkOptions, OrphanHandling, OutputFormat, OutputKind, ReportLevel, SortSection,
    UnresolvedSymbols, Visibility,
};
use crate::args::options::{CallGraphSort, Flavor};
use crate::args::response::{self, FileReader, FsReader};
use crate::args::table::{
    self, Action, ArgKind, DynFlag, OptionDef, PeAction, PeFlag, Status, ZAction, ZArg,
};
use crate::error::{Error, Result};

/// What a command line asked for.
#[derive(Clone, Debug)]
pub enum ParseOutcome {
    /// Perform a link with these options.
    Link(Box<LinkOptions>),
    /// Print usage and exit successfully.
    Help,
    /// Print the version line and exit successfully.
    Version,
}

/// Parses a GNU ld / gold / lld / mold command line.
///
/// `args` includes `argv[0]`, which also selects the flavor when it is a name
/// such as `ld64.qld` (see [`select_flavor`]). Response files are read with
/// [`std::fs::read`]; use [`parse_gnu_with`] to supply them another way.
///
/// The options describe a link run the way the `qld` binary runs one:
/// [`LinkOptions::use_process_defaults`] is applied, so the link map goes to
/// standard output and `LD_LIBRARY_PATH` and the other variables GNU ld
/// reads are taken from the environment. [`parse_gnu_with`] parses the same
/// command line into a hermetic, silent link.
///
/// Warnings found while parsing, such as unknown `-z` keywords, are returned
/// in [`LinkOptions::warnings`] for the caller to report.
///
/// # Errors
///
/// Returns [`Error::Option`] for an unknown, unsupported or malformed option,
/// [`Error::Io`] for an unreadable response file, and
/// [`Error::Unimplemented`] for a flavor qld does not parse yet.
pub fn parse_gnu<S: AsRef<OsStr>>(args: &[S]) -> Result<ParseOutcome> {
    process_defaults(parse_gnu_with(args, &FsReader)?)
}

/// Applies [`LinkOptions::use_process_defaults`] to a parsed link, which is
/// what makes [`parse_gnu`] and [`parse_darwin`] describe the binary's link
/// rather than a hermetic one.
fn process_defaults(outcome: ParseOutcome) -> Result<ParseOutcome> {
    Ok(match outcome {
        ParseOutcome::Link(mut options) => {
            options.use_process_defaults();
            ParseOutcome::Link(options)
        }
        other => other,
    })
}

/// Like [`parse_gnu`], but reads `@response` files through `reader`.
///
/// This performs no other I/O, so it is suitable for hermetic tests.
///
/// # Errors
///
/// As for [`parse_gnu`].
pub fn parse_gnu_with<S: AsRef<OsStr>>(
    args: &[S],
    reader: &dyn FileReader,
) -> Result<ParseOutcome> {
    let (flavor, first) = select_flavor(args)?;
    match flavor {
        Flavor::Darwin => parse_darwin_with(args, reader),
        Flavor::Gnu => {
            let raw: Vec<Vec<u8>> = args
                .get(first..)
                .unwrap_or_default()
                .iter()
                .map(|arg| arg.as_ref().as_encoded_bytes().to_vec())
                .collect();
            let quoting = response::quoting_from_args(&raw)?;
            let expanded = response::expand(raw, reader, quoting)?;
            GnuParser::new().run(expanded)
        }
    }
}

/// Parses an Apple ld64 command line.
///
/// `args` includes `argv[0]` (and may start with `-flavor darwin`).
/// Response files and `-filelist` files are read with [`std::fs::read`];
/// see [`crate::args::darwin`] for the option table. As in [`parse_gnu`],
/// the options carry [`LinkOptions::use_process_defaults`].
///
/// # Errors
///
/// Returns [`Error::Option`] for unknown, unsupported or malformed options
/// and [`Error::Io`] for unreadable response files.
pub fn parse_darwin<S: AsRef<OsStr>>(args: &[S]) -> Result<ParseOutcome> {
    process_defaults(parse_darwin_with(args, &FsReader)?)
}

/// Like [`parse_darwin`], but reads `@response` and `-filelist` files through
/// `reader`.
///
/// # Errors
///
/// As for [`parse_darwin`].
pub fn parse_darwin_with<S: AsRef<OsStr>>(
    args: &[S],
    reader: &dyn FileReader,
) -> Result<ParseOutcome> {
    let first = match args.get(1) {
        Some(arg) if arg.as_ref() == "-flavor" => 3,
        _ => 1,
    };
    let rest: Vec<&OsStr> = args
        .get(first..)
        .unwrap_or_default()
        .iter()
        .map(AsRef::as_ref)
        .collect();
    crate::args::darwin::parse(&rest, reader)
}

/// Chooses the command-line flavor.
///
/// An explicit `-flavor <name>` as the first argument wins; then the basename
/// of `argv[0]` (`ld64`, `ld64.*` select Darwin); then GNU. Returns the flavor
/// and the index of the first argument after `argv[0]` and any `-flavor`.
///
/// # Errors
///
/// Returns [`Error::Option`] for an unknown `-flavor` value, and
/// [`Error::Unimplemented`] for the MSVC (`link`, `lld-link`) and wasm
/// flavors.
pub fn select_flavor<S: AsRef<OsStr>>(args: &[S]) -> Result<(Flavor, usize)> {
    if let Some(first) = args.get(1)
        && first.as_ref() == "-flavor"
    {
        let Some(name) = args.get(2) else {
            return Err(Error::Option("missing argument to -flavor".into()));
        };
        let flavor = match name.as_ref().to_str() {
            Some("gnu" | "ld" | "elf") => Flavor::Gnu,
            Some("darwin" | "ld64" | "darwinnew") => Flavor::Darwin,
            Some("link" | "lld-link" | "msvc") => return Err(msvc_unimplemented()),
            Some("wasm" | "wasm-ld") => return Err(wasm_unimplemented()),
            _ => {
                return Err(Error::Option(format!(
                    "unknown flavor: {}",
                    name.as_ref().to_string_lossy()
                )));
            }
        };
        return Ok((flavor, 3));
    }

    let Some(argv0) = args.first() else {
        return Ok((Flavor::Gnu, 1));
    };
    let base = Path::new(argv0.as_ref())
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    if base == "ld64" || base.starts_with("ld64.") {
        return Ok((Flavor::Darwin, 1));
    }
    if base == "lld-link" || base == "link" {
        return Err(msvc_unimplemented());
    }
    if base == "wasm-ld" {
        return Err(wasm_unimplemented());
    }
    Ok((Flavor::Gnu, 1))
}

fn msvc_unimplemented() -> Error {
    Error::Unimplemented("the link.exe / lld-link command line (not scheduled)".into())
}

fn wasm_unimplemented() -> Error {
    Error::Unimplemented("the wasm-ld command line (not scheduled)".into())
}

/// Returns the `--help` text: every implemented option and `-z` keyword.
#[must_use]
pub fn usage() -> String {
    let mut text = format!(
        "Usage: qld [options] file...\n\
         \n\
         qld is a linker compatible with the GNU ld, gold, lld and mold command lines.\n\
         This is {version}, a pre-alpha build: linking is not implemented yet.\n\
         See ROADMAP.md.\n\
         \n\
         Multi-letter options take one or two dashes, except those starting with\n\
         'o', which need two. Options not listed here are either accepted and\n\
         ignored because they have no effect for qld, or rejected with an error.\n\
         \n\
         Options:\n",
        version = crate::version_line()
    );
    for def in table::GNU_OPTIONS {
        if def.status != Status::Implemented || def.help.is_empty() {
            continue;
        }
        let mut spellings = vec![spell(def)];
        spellings.extend(
            table::GNU_OPTIONS
                .iter()
                .filter(|other| {
                    other.status == Status::Implemented
                        && other.help.is_empty()
                        && other.action == def.action
                })
                .map(spell),
        );
        let mut left = spellings.join(", ");
        if !def.meta.is_empty() {
            match def.arg {
                ArgKind::OptionalValue => left.push_str(&format!("[={}]", def.meta)),
                ArgKind::EqualsValue => left.push_str(&format!("={}", def.meta)),
                ArgKind::JoinedValue => left.push_str(def.meta),
                _ => left.push_str(&format!(" {}", def.meta)),
            }
        }
        push_help_line(&mut text, &left, def.help);
    }
    text.push_str("\n-z keywords:\n");
    for keyword in table::Z_KEYWORDS {
        if keyword.status != Status::Implemented {
            continue;
        }
        let left = match keyword.arg {
            ZArg::Flag => format!("-z {}", keyword.name),
            ZArg::Value => format!("-z {}=VALUE", keyword.name),
        };
        push_help_line(&mut text, &left, keyword.help);
    }
    text.push_str("\n  @FILE                       Read options from FILE\n");
    // libtool decides whether the linker can build shared libraries by
    // looking for ": supported targets:.* elf" in `ld --help`. List only what
    // qld links today; later milestones extend these lines.
    text.push_str(
        "qld: supported targets: elf64-x86-64 elf64-littleaarch64 elf64-loongarch \
         pei-x86-64 pei-i386 pei-aarch64-little\n",
    );
    text.push_str(
        "qld: supported emulations: elf_x86_64 aarch64linux elf64loongarch \
         i386pep i386pe arm64pe\n",
    );
    text
}

fn push_help_line(text: &mut String, left: &str, help: &str) {
    const COLUMN: usize = 30;
    if left.len() + 2 < COLUMN {
        text.push_str(&format!("  {left:<width$}{help}\n", width = COLUMN - 2));
    } else {
        text.push_str(&format!("  {left}\n{:COLUMN$}{help}\n", ""));
    }
}

/// How an option is conventionally written in documentation.
fn spell(def: &OptionDef) -> String {
    const SINGLE_DASH: &[&str] = &[
        "soname",
        "rpath",
        "rpath-link",
        "dynamic-linker",
        "static",
        "shared",
        "pie",
        "no-pie",
        "plugin",
        "plugin-opt",
        "init",
        "fini",
        "nostdlib",
        "dT",
        "dn",
        "dy",
        "dc",
        "dp",
        "call_shared",
        "non_shared",
    ];
    let name = def.name;
    let single = name.len() == 1
        || name.starts_with(|c: char| c.is_ascii_uppercase())
        || SINGLE_DASH.contains(&name);
    if single {
        format!("-{name}")
    } else {
        format!("--{name}")
    }
}

/// Converts argument bytes back to an `OsString`, losslessly on Unix.
pub(crate) fn bytes_to_os(bytes: Vec<u8>) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        match String::from_utf8(bytes) {
            Ok(text) => OsString::from(text),
            Err(error) => OsString::from(String::from_utf8_lossy(error.as_bytes()).into_owned()),
        }
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A matched option and its value.
struct Matched {
    def: &'static OptionDef,
    /// The option as written, without its value (`--soname`, `-l`).
    spelling: String,
    value: Option<Vec<u8>>,
    /// Whether the value was taken from the following argument.
    separate: bool,
}

struct GnuParser {
    options: LinkOptions,
    attrs: InputAttrs,
    saved: Vec<InputAttrs>,
    shared: bool,
    pie: Option<bool>,
    relocatable: bool,
}

impl GnuParser {
    fn new() -> Self {
        Self {
            options: LinkOptions::new(),
            attrs: InputAttrs::default(),
            saved: Vec::new(),
            shared: false,
            pie: None,
            relocatable: false,
        }
    }

    fn run(mut self, args: Vec<Vec<u8>>) -> Result<ParseOutcome> {
        let mut args = args.into_iter();
        let mut only_inputs = false;
        while let Some(arg) = args.next() {
            if only_inputs {
                self.push_file(arg)?;
                continue;
            }
            if arg == b"--" {
                only_inputs = true;
                continue;
            }
            if arg.len() < 2 || !arg.starts_with(b"-") {
                self.push_file(arg)?;
                continue;
            }
            let matched = match_option(&arg, &mut args)?;
            match matched.def.status {
                Status::Implemented => {
                    if let Some(outcome) = self.apply(&matched)? {
                        return Ok(outcome);
                    }
                }
                Status::Ignored => {
                    let value = matched.value.filter(|_| matched.separate);
                    self.options.ignored.push(bytes_to_os(arg));
                    if let Some(value) = value {
                        self.options.ignored.push(bytes_to_os(value));
                    }
                }
                Status::Unsupported(why) => {
                    return Err(Error::Option(format!(
                        "unsupported option: {} ({why})",
                        matched.spelling
                    )));
                }
            }
        }
        self.finish()
    }

    /// Records a positional input.
    ///
    /// A file named `*.def` is a module-definition file rather than an
    /// object, as in GNU ld's PE emulations: it names the exports and the
    /// DLL, so it is kept aside instead of being handed to the input reader.
    fn push_file(&mut self, arg: Vec<u8>) -> Result<()> {
        let path = PathBuf::from(bytes_to_os(arg));
        if is_def_file(&path) {
            if let Some(first) = &self.options.pe.def_file {
                return Err(Error::Option(format!(
                    "only one .def file may be given: {} and {}",
                    first.display(),
                    path.display()
                )));
            }
            self.options.pe.def_file = Some(path);
            return Ok(());
        }
        self.options.push_input(InputKind::File(path), self.attrs);
        Ok(())
    }

    fn finish(mut self) -> Result<ParseOutcome> {
        if self.options.inputs.is_empty() {
            return Err(Error::Option("no input files".into()));
        }
        let static_at_end = self.attrs.static_only;
        self.options.kind = if self.relocatable {
            if self.shared {
                return Err(Error::Option(
                    "-r and -shared may not be used together".into(),
                ));
            }
            if self.pie == Some(true) {
                return Err(Error::Option("-r and -pie may not be used together".into()));
            }
            OutputKind::Relocatable
        } else if self.shared {
            OutputKind::Shared
        } else if self.pie == Some(true) {
            if static_at_end || self.options.no_dynamic_linker {
                OutputKind::StaticPie
            } else {
                OutputKind::Pie
            }
        } else if static_at_end {
            OutputKind::StaticExecutable
        } else {
            OutputKind::Executable
        };
        Ok(ParseOutcome::Link(Box::new(self.options)))
    }

    #[allow(clippy::too_many_lines)]
    fn apply(&mut self, m: &Matched) -> Result<Option<ParseOutcome>> {
        let o = &mut self.options;
        match m.def.action {
            Action::None | Action::RspQuoting => {}
            Action::Help => return Ok(Some(ParseOutcome::Help)),
            Action::Version => return Ok(Some(ParseOutcome::Version)),

            Action::Library => {
                let name = text(m)?;
                if name.is_empty() {
                    return Err(Error::Option(format!(
                        "{}: missing library name",
                        m.spelling
                    )));
                }
                let kind = match name.strip_prefix(':') {
                    Some(exact) => InputKind::LibraryExact(exact.to_owned()),
                    None => InputKind::Library(name),
                };
                o.push_input(kind, self.attrs);
            }
            Action::LibraryPath => o.search_paths.push(path(m)?),
            Action::Script => {
                let script = path(m)?;
                o.push_input(InputKind::Script(script), self.attrs);
            }
            Action::JustSymbols => {
                let file = path(m)?;
                o.push_input(InputKind::JustSymbols(file), self.attrs);
            }
            Action::Sysroot => o.sysroot = Some(path(m)?),
            Action::Nostdlib => o.nostdlib = true,
            Action::DefaultScript => o.default_script = Some(path(m)?),

            Action::WholeArchive(on) => self.attrs.whole_archive = on,
            Action::AsNeeded(on) => self.attrs.as_needed = on,
            Action::Static(on) => self.attrs.static_only = on,
            Action::HpuxLinkMode => {
                self.attrs.static_only = match text(m)?.as_str() {
                    "archive" => true,
                    "shared" | "default" => false,
                    other => {
                        return Err(Error::Option(format!(
                            "{}: unknown keyword: {other}",
                            m.spelling
                        )));
                    }
                };
            }
            Action::StartGroup => {
                if self.attrs.in_group {
                    return Err(Error::Option("nested --start-group".into()));
                }
                self.attrs.in_group = true;
            }
            Action::EndGroup => {
                if !self.attrs.in_group {
                    return Err(Error::Option("--end-group without --start-group".into()));
                }
                self.attrs.in_group = false;
            }
            Action::StartLib => {
                if self.attrs.lazy {
                    return Err(Error::Option("nested --start-lib".into()));
                }
                self.attrs.lazy = true;
            }
            Action::EndLib => {
                if !self.attrs.lazy {
                    return Err(Error::Option("--end-lib without --start-lib".into()));
                }
                self.attrs.lazy = false;
            }
            Action::PushState => self.saved.push(self.attrs),
            Action::PopState => {
                let Some(saved) = self.saved.pop() else {
                    return Err(Error::Option("--pop-state without --push-state".into()));
                };
                self.attrs = InputAttrs {
                    in_group: self.attrs.in_group,
                    lazy: self.attrs.lazy,
                    ..saved
                };
            }
            Action::CopyDtNeeded(on) => self.attrs.copy_dt_needed = on,
            Action::Format => {
                let name = text(m)?;
                self.attrs.format = match name.as_str() {
                    "binary" => InputFormat::Binary,
                    "default" | "elf" => InputFormat::Auto,
                    other if other.starts_with("elf") || other.starts_with("pe") => {
                        InputFormat::Auto
                    }
                    other => {
                        return Err(Error::Option(format!(
                            "unsupported input format for {}: {other}",
                            m.spelling
                        )));
                    }
                };
            }

            Action::Output => o.output = Some(path(m)?),
            Action::Emulation => {
                let name = text(m)?;
                match emulation::lookup(&name) {
                    Some(target) => o.target = Some(target),
                    None => {
                        let supported: Vec<&str> = emulation::EMULATIONS
                            .iter()
                            .map(|(name, _)| *name)
                            .collect();
                        return Err(Error::Option(format!(
                            "unrecognised emulation mode: {name} (supported: {})",
                            supported.join(" ")
                        )));
                    }
                }
            }
            Action::Endian(endian) => o.endian = Some(endian),
            Action::OutputFormat => o.output_format = Some(OutputFormat::from_name(&text(m)?)),
            Action::Shared => {
                self.shared = true;
                if self.pie == Some(true) {
                    self.pie = None;
                }
            }
            Action::Pie(on) => {
                self.pie = Some(on);
                if on {
                    self.shared = false;
                }
            }
            Action::Relocatable => self.relocatable = true,
            Action::DynamicLinker => o.dynamic_linker = Some(path(m)?),
            Action::NoDynamicLinker => o.no_dynamic_linker = true,
            Action::Entry => o.entry = Some(text(m)?),
            Action::Soname => o.soname = Some(text(m)?),
            Action::Rpath => o.rpaths.push(path(m)?),
            Action::RpathLink => o.rpath_links.push(path(m)?),
            Action::NewDtags(on) => o.new_dtags = Some(on),
            Action::Init => o.init = Some(text(m)?),
            Action::Fini => o.fini = Some(text(m)?),
            Action::Auxiliary => o.auxiliary.push(text(m)?),
            Action::Filter => o.filter.push(text(m)?),
            Action::SpareDynamicTags => o.spare_dynamic_tags = Some(integer(m)?),

            Action::Undefined => o.undefined.push(text(m)?),
            Action::UndefinedGlob => o.undefined_glob.push(text(m)?),
            Action::RequireDefined => o.require_defined.push(text(m)?),
            Action::Defsym => {
                let assignment = text(m)?;
                match assignment.split_once('=') {
                    Some((symbol, expr)) if !symbol.trim().is_empty() => o
                        .defsym
                        .push((symbol.trim().to_owned(), expr.trim().to_owned())),
                    _ => {
                        return Err(Error::Option(format!(
                            "{}: expected SYMBOL=EXPRESSION, got: {assignment}",
                            m.spelling
                        )));
                    }
                }
            }
            Action::Wrap => o.wrap.push(text(m)?),
            Action::TraceSymbol => o.trace_symbols.push(text(m)?),
            Action::ExportDynamic(on) => o.export_dynamic = on,
            Action::ExportDynamicSymbol => o.export_dynamic_symbols.push(text(m)?),
            Action::ExportDynamicSymbolList => o.export_dynamic_symbol_lists.push(path(m)?),
            Action::DynamicList => o.dynamic_lists.push(path(m)?),
            Action::ExcludeLibs => o.exclude_libs.extend(
                text(m)?
                    .split([',', ':'])
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
            ),
            Action::VersionScript => o.version_scripts.push(path(m)?),
            Action::UndefinedVersion(on) => o.undefined_version = Some(on),
            Action::DefaultSymver => o.default_symver = true,
            Action::Symbolic(mode) => o.symbolic = mode,
            Action::NoUndefined => o.no_undefined = Some(true),
            Action::AllowShlibUndefined(on) => o.allow_shlib_undefined = Some(on),
            Action::UnresolvedSymbols => {
                o.unresolved_symbols = Some(match text(m)?.as_str() {
                    "ignore-all" => UnresolvedSymbols::IgnoreAll,
                    "report-all" => UnresolvedSymbols::ReportAll,
                    "ignore-in-object-files" => UnresolvedSymbols::IgnoreInObjectFiles,
                    "ignore-in-shared-libs" => UnresolvedSymbols::IgnoreInSharedLibs,
                    other => return Err(bad_value(m, other)),
                });
            }
            Action::WarnUnresolvedSymbols(on) => o.warn_unresolved_symbols = on,
            Action::IgnoreUnresolvedSymbol => o.ignore_unresolved_symbols.push(text(m)?),
            Action::AllowMultipleDefinition(on) => o.allow_multiple_definition = on,
            Action::DefineCommon => o.define_common = true,
            Action::RetainSymbolsFile => o.retain_symbols_file = Some(path(m)?),

            Action::GcSections(on) => o.gc_sections = on,
            Action::PrintGcSections(on) => o.print_gc_sections = on,
            Action::GcKeepExported => o.gc_keep_exported = true,
            Action::WhyLive => o.why_live.push(text(m)?),
            Action::Icf => {
                o.icf = match text(m)?.as_str() {
                    "none" => IcfMode::None,
                    "safe" => IcfMode::Safe,
                    "all" => IcfMode::All,
                    other => return Err(bad_value(m, other)),
                };
            }
            Action::PrintIcfSections(on) => o.print_icf_sections = on,
            Action::KeepUnique => o.keep_unique.push(text(m)?),
            Action::IgnoreDataAddressEquality => o.ignore_data_address_equality = true,
            Action::IgnoreFunctionAddressEquality => o.ignore_function_address_equality = true,
            Action::Strip(mode) => o.strip = o.strip.max(mode),
            Action::Discard(mode) => o.discard = mode,
            Action::EmitRelocs => o.emit_relocs = true,
            Action::Magic(mode) => o.magic = mode,
            Action::Relax(on) => o.relax = on,
            Action::RelaxGp(on) => o.relax_gp = on,
            Action::ImageBase => o.image_base = Some(integer(m)?),
            Action::SectionStart => {
                let spec = text(m)?;
                let Some((section, address)) = spec.split_once('=') else {
                    return Err(Error::Option(format!(
                        "{}: expected SECTION=ADDRESS, got: {spec}",
                        m.spelling
                    )));
                };
                let address = parse_hex(address).ok_or_else(|| bad_value(m, &spec))?;
                o.section_starts.push((section.to_owned(), address));
            }
            Action::SectionStartNamed(section) => {
                let address = hex(m)?;
                o.section_starts.push((section.to_owned(), address));
            }
            Action::TextSegment => o.text_segment = Some(hex(m)?),
            Action::RodataSegment => o.rodata_segment = Some(hex(m)?),
            Action::LdataSegment => o.ldata_segment = Some(hex(m)?),
            Action::OrphanHandling => {
                o.orphan_handling =
                    match one_of(m, &["place", "warn", "error", "discard"])?.as_str() {
                        "warn" => OrphanHandling::Warn,
                        "error" => OrphanHandling::Error,
                        "discard" => OrphanHandling::Discard,
                        _ => OrphanHandling::Place,
                    };
            }
            Action::SortSection => {
                o.sort_section = match one_of(m, &["name", "alignment"])?.as_str() {
                    "alignment" => SortSection::Alignment,
                    _ => SortSection::Name,
                };
            }
            Action::Rosegment(on) => o.rosegment = Some(on),
            Action::EhFrameHdr(on) => o.eh_frame_hdr = on,
            Action::BuildId => o.build_id = build_id(m)?,
            Action::HashStyle => {
                o.hash_style = match text(m)?.as_str() {
                    "sysv" => HashStyle::Sysv,
                    "gnu" => HashStyle::Gnu,
                    "both" => HashStyle::Both,
                    other => return Err(bad_value(m, other)),
                };
            }
            Action::CompressDebugSections => {
                o.compress_debug_sections =
                    match one_of(m, &["none", "zlib", "zlib-gnu", "zlib-gabi", "zstd"])?.as_str() {
                        "zlib" => DebugCompression::Zlib,
                        "zlib-gnu" => DebugCompression::ZlibGnu,
                        "zlib-gabi" => DebugCompression::ZlibGabi,
                        "zstd" => DebugCompression::Zstd,
                        _ => DebugCompression::None,
                    };
            }
            Action::PackageMetadata => {
                o.package_metadata = match &m.value {
                    Some(_) => Some(text(m)?),
                    None => None,
                };
            }
            Action::SymbolOrderingFile => o.symbol_ordering_file = Some(path(m)?),
            Action::WarnSymbolOrdering(on) => o.no_warn_symbol_ordering = !on,
            Action::CallGraphProfileSort => {
                o.call_graph_profile_sort = Some(match &m.value {
                    // The flag alone (lld 16 and older) meant hfsort.
                    None => CallGraphSort::Hfsort,
                    Some(_) => match text(m)?.as_str() {
                        "none" => CallGraphSort::None,
                        "hfsort" => CallGraphSort::Hfsort,
                        "cdsort" => CallGraphSort::Cdsort,
                        other => return Err(bad_value(m, other)),
                    },
                });
            }
            Action::NoCallGraphProfileSort => o.call_graph_profile_sort = Some(CallGraphSort::None),
            Action::CallGraphOrderingFile => o.call_graph_ordering_file = Some(path(m)?),
            Action::PrintSymbolOrder => o.print_symbol_order = Some(path(m)?),
            Action::GdbIndex(on) => o.gdb_index = on,
            Action::S390Pgste => o.s390_pgste = true,
            Action::DebugNames(on) => o.debug_names = on,
            Action::SeparateDebugFile(on) => {
                o.separate_debug_file = match (&m.value, on) {
                    (_, false) => None,
                    (None, true) => Some(None),
                    (Some(_), true) => Some(Some(path(m)?)),
                };
            }
            Action::PackDynRelocs => {
                o.pack_relative_relocs = match text(m)?.as_str() {
                    "none" => false,
                    "relr" => true,
                    other @ ("android" | "android+relr") => {
                        return Err(Error::Option(format!(
                            "unsupported option: {}={other} (Android packed relocations are not supported)",
                            m.spelling
                        )));
                    }
                    other => return Err(bad_value(m, other)),
                };
            }
            Action::ApplyDynamicRelocs(on) => o.apply_dynamic_relocs = on,
            Action::FixCortexA53Erratum843419 => o.fix_cortex_a53_843419 = true,
            Action::FixCortexA53Erratum835769 => o.aarch64.fix_cortex_a53_835769 = true,
            Action::Z => {
                let keyword = required(m)?.to_vec();
                self.apply_z(&keyword)?;
            }

            Action::Optimize => {
                let level = text(m)?;
                let level: u64 = level.parse().map_err(|_| bad_value(m, &level))?;
                o.optimize = u8::try_from(level).unwrap_or(u8::MAX);
            }
            Action::Threads => {
                o.threads = match &m.value {
                    None => None,
                    Some(_) => {
                        let count = integer(m)?;
                        match usize::try_from(count) {
                            Ok(count) if count > 0 => Some(count),
                            _ => return Err(bad_value(m, &text(m)?)),
                        }
                    }
                };
            }
            Action::NoThreads => o.threads = Some(1),
            Action::Fork(on) => o.fork = on,
            Action::MapFile => o.map_file = Some(path(m)?),
            Action::PrintMap => o.print_map = true,
            Action::Cref => o.cref = true,
            Action::Trace => o.trace = true,
            Action::Verbose => o.verbose = true,
            Action::Demangle(on) => o.demangle = on,
            Action::FatalWarnings(on) => o.fatal_warnings = on,
            Action::NoWarnings => o.no_warnings = true,
            Action::ErrorLimit => o.error_limit = Some(integer(m)?),
            Action::Color => {
                o.color = match &m.value {
                    None => ColorChoice::Always,
                    Some(_) => match text(m)?.as_str() {
                        "auto" => ColorChoice::Auto,
                        "always" => ColorChoice::Always,
                        "never" => ColorChoice::Never,
                        other => return Err(bad_value(m, other)),
                    },
                };
            }
            Action::NoColor => o.color = ColorChoice::Never,
            Action::WarnCommon(on) => o.warn_common = on,
            Action::WarnBackrefs(on) => o.warn_backrefs = on,
            Action::WarnBackrefsExclude => o.warn_backrefs_exclude.push(text(m)?),
            Action::WarnTextrel => o.warn_textrel = true,
            Action::NoinhibitExec => o.noinhibit_exec = true,
            Action::DependencyFile => o.dependency_file = Some(path(m)?),
            Action::DependentLibraries(on) => o.dependent_libraries = on,
            Action::Plugin => o.plugins.push((path(m)?, Vec::new())),
            Action::PluginSaveTemps => o.plugin_save_temps = true,
            Action::PluginOpt => {
                let option = text(m)?;
                match o.plugins.last_mut() {
                    Some((_, plugin_options)) => plugin_options.push(option),
                    None => {
                        o.warnings
                            .push(format!("{} {option} ignored: no -plugin given", m.spelling));
                        o.ignored
                            .push(OsString::from(format!("{}={option}", m.spelling)));
                    }
                }
            }

            Action::Pe(action) => self.apply_pe(m, action)?,
        }
        Ok(None)
    }

    /// Applies one PE/COFF option.
    ///
    /// These are per-emulation options in GNU ld: they parse whatever the
    /// target is, and only a PE link reads them.
    fn apply_pe(&mut self, m: &Matched, action: PeAction) -> Result<()> {
        let pe = &mut self.options.pe;
        // Options whose default depends on the emulation (W33).
        match action {
            PeAction::Flag(PeFlag::LargeAddressAware, _) => pe.explicit.large_address_aware = true,
            PeAction::MajorOsVersion | PeAction::MinorOsVersion => pe.explicit.os_version = true,
            PeAction::MajorImageVersion | PeAction::MinorImageVersion => {
                pe.explicit.image_version = true;
            }
            PeAction::MajorSubsystemVersion | PeAction::MinorSubsystemVersion => {
                pe.explicit.subsystem_version = true;
            }
            _ => {}
        }
        match action {
            PeAction::Flag(flag, on) => match flag {
                PeFlag::Dynamicbase => pe.dynamicbase = on,
                PeFlag::Nxcompat => pe.nxcompat = on,
                PeFlag::HighEntropyVa => pe.high_entropy_va = on,
                PeFlag::Tsaware => pe.tsaware = on,
                PeFlag::NoSeh => pe.no_seh = on,
                PeFlag::ForceInteg => pe.forceinteg = on,
                PeFlag::NoIsolation => pe.no_isolation = on,
                PeFlag::NoBind => pe.no_bind = on,
                PeFlag::WdmDriver => pe.wdmdriver = on,
                PeFlag::LargeAddressAware => pe.large_address_aware = on,
                PeFlag::RelocSection => pe.reloc_section = on,
                PeFlag::InsertTimestamp => pe.insert_timestamp = on,
                PeFlag::ExportAllSymbols => pe.export_all_symbols = on,
                PeFlag::ExcludeAllSymbols => pe.exclude_all_symbols = on,
                PeFlag::KillAt => pe.kill_at = on,
                PeFlag::AddStdcallAlias => pe.add_stdcall_alias = on,
                PeFlag::StdcallFixup => pe.stdcall_fixup = Some(on),
                PeFlag::AutoImport => pe.auto_import = on,
                PeFlag::RuntimePseudoReloc => pe.runtime_pseudo_reloc = on,
                PeFlag::WarnDuplicateExports => pe.warn_duplicate_exports = on,
            },
            PeAction::Subsystem => {
                let value = text(m)?;
                let (subsystem, version) = crate::coff::options::parse_subsystem(&value)?;
                pe.subsystem = Some(subsystem);
                if let Some(version) = version {
                    pe.explicit.subsystem_version = true;
                    pe.major_subsystem_version = version.major;
                    pe.minor_subsystem_version = version.minor;
                }
            }
            PeAction::SectionAlignment => pe.section_alignment = alignment(m)?,
            PeAction::FileAlignment => pe.file_alignment = alignment(m)?,
            PeAction::Stack => pe.stack = reserve_and_commit(m, pe.stack)?,
            PeAction::Heap => pe.heap = reserve_and_commit(m, pe.heap)?,
            PeAction::MajorImageVersion => pe.major_image_version = version_field(m)?,
            PeAction::MinorImageVersion => pe.minor_image_version = version_field(m)?,
            PeAction::MajorOsVersion => pe.major_os_version = version_field(m)?,
            PeAction::MinorOsVersion => pe.minor_os_version = version_field(m)?,
            PeAction::MajorSubsystemVersion => pe.major_subsystem_version = version_field(m)?,
            PeAction::MinorSubsystemVersion => pe.minor_subsystem_version = version_field(m)?,
            PeAction::OutImplib => pe.out_implib = Some(path(m)?),
            PeAction::OutputDef => pe.output_def = Some(path(m)?),
            PeAction::ExcludeSymbols => pe.exclude_symbols.extend(comma_list(m)?),
            PeAction::ExcludeModulesForImplib => {
                pe.exclude_modules_for_implib.extend(comma_list(m)?);
            }
            PeAction::Export => pe.exports.push(text(m)?),
        }
        Ok(())
    }

    fn apply_z(&mut self, keyword: &[u8]) -> Result<()> {
        let written = lossy(keyword);
        let (name, value) = match written.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (written.as_str(), None),
        };
        let found = table::find_z(name).filter(|z| match z.arg {
            ZArg::Flag => value.is_none(),
            ZArg::Value => value.is_some(),
        });
        let Some(z) = found else {
            self.options.warnings.push(format!("-z {written} ignored"));
            return Ok(());
        };
        if std::str::from_utf8(keyword).is_err() {
            return Err(Error::Option(format!(
                "-z {written}: value is not valid UTF-8"
            )));
        }
        match z.status {
            Status::Implemented => {}
            Status::Ignored => {
                self.options.ignored.push(OsString::from("-z"));
                self.options.ignored.push(OsString::from(written.clone()));
                return Ok(());
            }
            Status::Unsupported(why) => {
                return Err(Error::Option(format!(
                    "unsupported option: -z {name} ({why})"
                )));
            }
        }
        let value = value.unwrap_or_default();
        let bad = || Error::Option(format!("invalid value for -z {name}: {value}"));
        let o = &mut self.options;
        match z.action {
            ZAction::None => {}
            ZAction::Now(on) => o.bind_now = on,
            ZAction::Relro(on) => o.relro = on,
            ZAction::Defs(on) => o.no_undefined = Some(on),
            ZAction::Muldefs => o.allow_multiple_definition = true,
            ZAction::ExecStack(mode) => o.exec_stack = mode,
            ZAction::GnuStack(on) => o.gnu_stack = on,
            ZAction::SeparateCode(mode) => o.separate_code = Some(mode),
            ZAction::MaxPageSize => {
                o.max_page_size = Some(
                    parse_int(value)
                        .filter(|v| v.is_power_of_two())
                        .ok_or_else(bad)?,
                );
            }
            ZAction::CommonPageSize => {
                o.common_page_size = Some(
                    parse_int(value)
                        .filter(|v| v.is_power_of_two())
                        .ok_or_else(bad)?,
                );
            }
            ZAction::StackSize => {
                let size = parse_int(value).ok_or_else(bad)?;
                o.stack_size = Some(size);
                // PE has no PT_GNU_STACK; the reserve in the optional header
                // is what `-z stack-size` means there, and `--stack` sets the
                // same field, so the last one written wins.
                o.pe.stack.0 = size;
            }
            ZAction::CopyReloc(on) => o.copy_relocs = on,
            ZAction::CombReloc(on) => o.combine_relocs = on,
            ZAction::PackRelativeRelocs(on) => o.pack_relative_relocs = on,
            ZAction::Text(on) => o.error_textrel = on,
            ZAction::DynFlag(flag) => {
                let flags = &mut o.dynamic_flags;
                match flag {
                    DynFlag::Nodelete => flags.nodelete = true,
                    DynFlag::Nodlopen => flags.nodlopen = true,
                    DynFlag::Nodump => flags.nodump = true,
                    DynFlag::Initfirst => flags.initfirst = true,
                    DynFlag::Interpose => flags.interpose = true,
                    DynFlag::Global => flags.global = true,
                    DynFlag::Nodefaultlib => flags.nodefaultlib = true,
                    DynFlag::Loadfltr => flags.loadfltr = true,
                    DynFlag::Origin => flags.origin = true,
                    DynFlag::Singleton(on) => flags.singleton = on,
                }
            }
            ZAction::StartStopGc(on) => o.start_stop_gc = Some(on),
            ZAction::StartStopVisibility => {
                o.start_stop_visibility = Some(match value.to_ascii_lowercase().as_str() {
                    "default" => Visibility::Default,
                    "internal" => Visibility::Internal,
                    "hidden" => Visibility::Hidden,
                    "protected" => Visibility::Protected,
                    _ => return Err(bad()),
                });
            }
            ZAction::KeepTextSectionPrefix(on) => o.keep_text_section_prefix = on,
            ZAction::Ibt => o.x86.ibt = true,
            ZAction::Shstk => o.x86.shstk = true,
            ZAction::IbtPlt => o.x86.ibtplt = true,
            ZAction::ForceBti => o.aarch64.force_bti = true,
            ZAction::PacPlt => o.aarch64.pac_plt = true,
            ZAction::CetReport => {
                o.x86.cet_report = match value {
                    "none" => ReportLevel::None,
                    "warning" => ReportLevel::Warning,
                    "error" => ReportLevel::Error,
                    _ => return Err(bad()),
                };
            }
            ZAction::IsaLevel(level) => o.x86.isa_level = level,
            ZAction::DynamicUndefinedWeak(on) => o.dynamic_undefined_weak = Some(on),
            ZAction::ExternProtectedData(on) => o.extern_protected_data = on,
            ZAction::MarkPlt(on) => o.mark_plt = on,
            ZAction::SectionHeader(on) => o.section_header = on,
            ZAction::MemorySeal(on) => o.memory_seal = on,
            ZAction::DeadRelocInNonalloc => {
                let (glob, value) = crate::debug::tombstone::parse_rule(value).ok_or_else(bad)?;
                o.dead_reloc_in_nonalloc.push((glob.to_owned(), value));
            }
        }
        Ok(())
    }
}

/// Matches `arg` (which starts with `-` and is at least two bytes long)
/// against the option table, taking a separate value from `rest` if needed.
fn match_option(arg: &[u8], rest: &mut impl Iterator<Item = Vec<u8>>) -> Result<Matched> {
    let (body, two_dashes) = match arg.strip_prefix(b"--") {
        Some(body) => (body, true),
        None => (arg.get(1..).unwrap_or_default(), false),
    };
    let dashes = if two_dashes { "--" } else { "-" };
    let unknown = || Error::Option(format!("unknown option: {}", lossy(arg)));

    // Long options: one or two dashes, but two for names starting with 'o'.
    if two_dashes || (body.len() > 1 && !body.starts_with(b"o")) {
        let (key, inline) = match body.iter().position(|&b| b == b'=') {
            Some(eq) => (
                body.get(..eq).unwrap_or_default(),
                body.get(eq + 1..).map(<[u8]>::to_vec),
            ),
            None => (body, None),
        };
        if key.len() > 1
            && let Some(def) = table::find_bytes(key)
        {
            let spelling = format!("{dashes}{}", def.name);
            return take_value(def, spelling, inline, rest);
        }
        if let Some(def) = table::joined_options()
            .filter(|def| body.starts_with(def.name.as_bytes()) && body.len() > def.name.len())
            .max_by_key(|def| def.name.len())
        {
            return Ok(Matched {
                def,
                spelling: format!("{dashes}{}", def.name),
                value: body.get(def.name.len()..).map(<[u8]>::to_vec),
                separate: false,
            });
        }
        if two_dashes {
            return Err(unknown());
        }
    }

    // Short options: `-x`, `-xVALUE`, `-x VALUE`.
    let Some((&first, joined)) = body.split_first() else {
        return Err(unknown());
    };
    let def = table::find_bytes(&[first]).ok_or_else(unknown)?;
    let spelling = format!("-{}", def.name);
    match def.arg {
        ArgKind::Flag if joined.is_empty() => Ok(Matched {
            def,
            spelling,
            value: None,
            separate: false,
        }),
        ArgKind::Value if !joined.is_empty() => Ok(Matched {
            def,
            spelling,
            value: Some(joined.to_vec()),
            separate: false,
        }),
        ArgKind::Value => take_value(def, spelling, None, rest),
        _ => Err(unknown()),
    }
}

fn take_value(
    def: &'static OptionDef,
    spelling: String,
    inline: Option<Vec<u8>>,
    rest: &mut impl Iterator<Item = Vec<u8>>,
) -> Result<Matched> {
    let (value, separate) = match (def.arg, inline) {
        (ArgKind::Flag, None) | (ArgKind::OptionalValue, None) => (None, false),
        (ArgKind::Flag, Some(_)) => {
            return Err(Error::Option(format!(
                "option does not take a value: {spelling}"
            )));
        }
        (ArgKind::Value, None) => match rest.next() {
            Some(value) => (Some(value), true),
            None => return Err(missing(&spelling)),
        },
        (ArgKind::EqualsValue | ArgKind::JoinedValue, None) => return Err(missing(&spelling)),
        (_, Some(value)) => (Some(value), false),
    };
    Ok(Matched {
        def,
        spelling,
        value,
        separate,
    })
}

/// Whether `path` names a module-definition file (`*.def`, in any case).
fn is_def_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.as_encoded_bytes().eq_ignore_ascii_case(b"def"))
}

fn missing(spelling: &str) -> Error {
    Error::Option(format!("missing argument to {spelling}"))
}

fn bad_value(m: &Matched, value: &str) -> Error {
    Error::Option(format!("invalid value for {}: {value}", m.spelling))
}

fn required(m: &Matched) -> Result<&[u8]> {
    m.value.as_deref().ok_or_else(|| missing(&m.spelling))
}

fn text(m: &Matched) -> Result<String> {
    String::from_utf8(required(m)?.to_vec())
        .map_err(|_| Error::Option(format!("{}: value is not valid UTF-8", m.spelling)))
}

fn path(m: &Matched) -> Result<PathBuf> {
    Ok(response::bytes_to_path(required(m)?))
}

fn integer(m: &Matched) -> Result<u64> {
    let value = text(m)?;
    parse_int(&value).ok_or_else(|| bad_value(m, &value))
}

fn hex(m: &Matched) -> Result<u64> {
    let value = text(m)?;
    parse_hex(&value).ok_or_else(|| bad_value(m, &value))
}

/// A PE alignment: a `strtoul`-style integer that fits in 32 bits.
fn alignment(m: &Matched) -> Result<u32> {
    let value = integer(m)?;
    u32::try_from(value).map_err(|_| bad_value(m, &text(m).unwrap_or_default()))
}

/// A PE version field: a `strtoul`-style integer that fits in 16 bits.
fn version_field(m: &Matched) -> Result<u16> {
    let value = integer(m)?;
    u16::try_from(value).map_err(|_| bad_value(m, &text(m).unwrap_or_default()))
}

/// `--stack` and `--heap`: `RESERVE[,COMMIT]`, keeping `current`'s commit
/// size when only a reserve is given, as GNU ld's PE emulations do.
fn reserve_and_commit(m: &Matched, current: (u64, u64)) -> Result<(u64, u64)> {
    let value = text(m)?;
    let (reserve, commit) = match value.split_once(',') {
        Some((reserve, commit)) => (reserve, Some(commit)),
        None => (value.as_str(), None),
    };
    let reserve = parse_int(reserve).ok_or_else(|| bad_value(m, &value))?;
    let commit = match commit {
        Some(commit) => parse_int(commit).ok_or_else(|| bad_value(m, &value))?,
        None => current.1,
    };
    Ok((reserve, commit))
}

/// A comma-separated list of names, as `--exclude-symbols` takes.
fn comma_list(m: &Matched) -> Result<Vec<String>> {
    Ok(text(m)?
        .split([',', ' '])
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

fn one_of(m: &Matched, allowed: &[&str]) -> Result<String> {
    let value = text(m)?;
    if allowed.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(bad_value(m, &value))
    }
}

/// Parses an integer the way `strtoul(s, 0)` does: `0x` hex, leading-`0`
/// octal, otherwise decimal.
fn parse_int(value: &str) -> Option<u64> {
    let value = value.trim();
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else if value.len() > 1
        && let Some(octal) = value.strip_prefix('0')
    {
        u64::from_str_radix(octal, 8).ok()
    } else {
        value.parse().ok()
    }
}

/// Parses a hexadecimal address, with or without `0x`, as GNU ld does for
/// `-Ttext` and `--section-start`.
fn parse_hex(value: &str) -> Option<u64> {
    let value = value.trim();
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    u64::from_str_radix(digits, 16).ok()
}

fn build_id(m: &Matched) -> Result<BuildId> {
    if m.value.is_none() {
        return Ok(BuildId::Sha1);
    }
    let style = text(m)?;
    Ok(match style.as_str() {
        "none" => BuildId::None,
        "fast" => BuildId::Fast,
        "md5" => BuildId::Md5,
        "sha1" | "tree" => BuildId::Sha1,
        "uuid" => BuildId::Uuid,
        other => {
            let digits = other
                .strip_prefix("0x")
                .or_else(|| other.strip_prefix("0X"))
                .ok_or_else(|| bad_value(m, other))?;
            decode_hex(digits).ok_or_else(|| bad_value(m, other))?
        }
    })
}

fn decode_hex(digits: &str) -> Option<BuildId> {
    let digits: Vec<u8> = digits.bytes().filter(|&b| b != b'-' && b != b':').collect();
    let (pairs, odd) = digits.as_chunks::<2>();
    if pairs.is_empty() || !odd.is_empty() {
        return None;
    }
    let bytes = pairs
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect::<Option<Vec<u8>>>()?;
    Some(BuildId::Hex(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(args: &[&str]) -> Box<LinkOptions> {
        let mut argv = vec!["ld"];
        argv.extend_from_slice(args);
        match parse_gnu_with(&argv, &response::NoFiles) {
            Ok(ParseOutcome::Link(options)) => options,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn recognizes_help_and_version() {
        assert!(matches!(
            parse_gnu(&["qld", "--help"]).unwrap(),
            ParseOutcome::Help
        ));
        assert!(matches!(
            parse_gnu(&["qld", "--version"]).unwrap(),
            ParseOutcome::Version
        ));
        assert!(matches!(
            parse_gnu(&["qld", "-v"]).unwrap(),
            ParseOutcome::Version
        ));
    }

    #[test]
    fn version_line_is_detected_as_gnu_compatible() {
        // autoconf and libtool look for "GNU" in `ld -v` output.
        assert!(crate::version_line().contains("GNU"));
    }

    #[test]
    fn empty_command_line_is_an_error() {
        assert!(parse_gnu(&["qld"]).is_err());
    }

    #[test]
    fn integers_follow_strtoul() {
        assert_eq!(parse_int("0x1000"), Some(0x1000));
        assert_eq!(parse_int("010"), Some(8));
        assert_eq!(parse_int("0"), Some(0));
        assert_eq!(parse_int("4096"), Some(4096));
        assert_eq!(parse_int("09"), None);
        assert_eq!(parse_int("x"), None);
        assert_eq!(parse_hex("1000"), Some(0x1000));
        assert_eq!(parse_hex("0x400000"), Some(0x40_0000));
        assert_eq!(parse_hex("g"), None);
    }

    #[test]
    fn short_option_values_and_long_equals() {
        let o = link(&["-soname=libx.so", "-hliby.so", "a.o"]);
        assert_eq!(o.soname.as_deref(), Some("liby.so"));
        let o = link(&["-omagic", "a.o"]);
        assert_eq!(o.output, Some(PathBuf::from("magic")));
    }

    #[test]
    fn usage_lists_implemented_options_only() {
        let help = usage();
        assert!(help.contains("--gc-sections"));
        assert!(help.contains("-Bstatic, -static, -dn, -non_shared"));
        assert!(help.contains("-z now"));
        assert!(!help.contains("--incremental"));
    }
}
