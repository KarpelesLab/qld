//! The GNU-flavor option table.
//!
//! [`GNU_OPTIONS`] lists every option of GNU ld 2.4x (ELF and PE emulations),
//! gold, lld and mold that qld recognizes, and [`Z_KEYWORDS`] lists the `-z`
//! keywords. Each entry has a [`Status`], following the policy in
//! `docs/compatibility.md`:
//!
//! - [`Status::Implemented`]: the parser records the option in
//!   [`LinkOptions`](crate::args::LinkOptions). Later stages are responsible
//!   for honoring every field they read, or for failing with
//!   [`Error::Unimplemented`](crate::Error::Unimplemented).
//! - [`Status::Ignored`]: parsed and dropped, because it has no observable
//!   effect for qld. The option text is kept in `LinkOptions::ignored`.
//! - [`Status::Unsupported`]: an error that names the option, because
//!   ignoring it could produce a wrong binary.
//!
//! Spelling rules (applied by the parser, not stored per entry): a
//! one-character name takes one dash; a longer name takes one or two dashes,
//! except names starting with `o`, which need two.

use crate::args::options::{DiscardMode, MagicMode, SeparateCode, StripMode, SymbolicMode};
use crate::target::Endianness;

/// How an option takes its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// No value. `--flag=value` is an error.
    Flag,
    /// A required value. One-character options take it joined or as the next
    /// argument (`-lc`, `-l c`); longer options take it after `=` or as the
    /// next argument (`--soname=x`, `--soname x`).
    Value,
    /// An optional value, only accepted after `=` (`--build-id`,
    /// `--build-id=md5`).
    OptionalValue,
    /// A required value, only accepted after `=` (`--why-extract=file`).
    EqualsValue,
    /// A value joined directly to the name, with no separator
    /// (`--lto-O2`).
    JoinedValue,
}

/// What qld does with an option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Recorded in `LinkOptions`.
    Implemented,
    /// Accepted with no effect.
    Ignored,
    /// Rejected with an error. The string says what qld lacks.
    Unsupported(&'static str),
}

/// One command-line option.
#[derive(Clone, Copy, Debug)]
pub struct OptionDef {
    /// The name without leading dashes, such as `soname` or `l`.
    pub name: &'static str,
    /// How the option takes a value.
    pub arg: ArgKind,
    /// Whether qld implements, ignores or rejects it.
    pub status: Status,
    /// Placeholder for the value in `--help`, such as `FILE`.
    pub meta: &'static str,
    /// One-line description for `--help`. Empty for aliases and for options
    /// that are not implemented.
    pub help: &'static str,
    pub(crate) action: Action,
}

/// How a `-z` keyword takes a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZArg {
    /// A bare keyword: `-z now`.
    Flag,
    /// `keyword=value`: `-z max-page-size=4096`.
    Value,
}

/// One `-z` keyword.
#[derive(Clone, Copy, Debug)]
pub struct ZKeyword {
    /// The keyword, without any `=value`.
    pub name: &'static str,
    /// Whether it takes a value.
    pub arg: ZArg,
    /// Whether qld implements, ignores or rejects it.
    pub status: Status,
    /// One-line description for `--help`.
    pub help: &'static str,
    pub(crate) action: ZAction,
}

/// What an implemented option does. Private to the parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    None,
    Help,
    Version,
    RspQuoting,
    // Inputs and search paths.
    Library,
    LibraryPath,
    Script,
    JustSymbols,
    Sysroot,
    Nostdlib,
    DefaultScript,
    // Positional state.
    WholeArchive(bool),
    AsNeeded(bool),
    Static(bool),
    HpuxLinkMode,
    StartGroup,
    EndGroup,
    StartLib,
    EndLib,
    PushState,
    PopState,
    CopyDtNeeded(bool),
    Format,
    // Output.
    Output,
    Emulation,
    Endian(Endianness),
    OutputFormat,
    Shared,
    Pie(bool),
    Relocatable,
    DynamicLinker,
    NoDynamicLinker,
    Entry,
    Soname,
    Rpath,
    RpathLink,
    NewDtags(bool),
    Init,
    Fini,
    Auxiliary,
    Filter,
    SpareDynamicTags,
    // Symbols.
    Undefined,
    UndefinedGlob,
    RequireDefined,
    Defsym,
    Wrap,
    TraceSymbol,
    ExportDynamic(bool),
    ExportDynamicSymbol,
    ExportDynamicSymbolList,
    DynamicList,
    ExcludeLibs,
    VersionScript,
    UndefinedVersion(bool),
    DefaultSymver,
    Symbolic(SymbolicMode),
    NoUndefined,
    AllowShlibUndefined(bool),
    UnresolvedSymbols,
    WarnUnresolvedSymbols(bool),
    IgnoreUnresolvedSymbol,
    AllowMultipleDefinition(bool),
    DefineCommon,
    RetainSymbolsFile,
    // Sections and layout.
    GcSections(bool),
    PrintGcSections(bool),
    GcKeepExported,
    WhyLive,
    Icf,
    PrintIcfSections(bool),
    KeepUnique,
    IgnoreDataAddressEquality,
    IgnoreFunctionAddressEquality,
    Strip(StripMode),
    Discard(DiscardMode),
    EmitRelocs,
    Magic(MagicMode),
    Relax(bool),
    ImageBase,
    SectionStart,
    SectionStartNamed(&'static str),
    TextSegment,
    RodataSegment,
    LdataSegment,
    OrphanHandling,
    SortSection,
    Rosegment(bool),
    EhFrameHdr(bool),
    BuildId,
    HashStyle,
    CompressDebugSections,
    PackageMetadata,
    PackDynRelocs,
    ApplyDynamicRelocs(bool),
    FixCortexA53Erratum843419,
    Z,
    // Diagnostics and driver behavior.
    Optimize,
    Threads,
    NoThreads,
    Fork(bool),
    MapFile,
    PrintMap,
    Cref,
    Trace,
    Verbose,
    Demangle(bool),
    FatalWarnings(bool),
    NoWarnings,
    ErrorLimit,
    Color,
    NoColor,
    WarnCommon(bool),
    WarnBackrefs(bool),
    WarnBackrefsExclude,
    WarnTextrel,
    NoinhibitExec,
    DependencyFile,
    DependentLibraries(bool),
    Plugin,
    PluginOpt,
    PluginSaveTemps,
    // PE/COFF (MinGW emulations).
    Pe(PeAction),
}

/// What an implemented PE/COFF option does. Private to the parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeAction {
    /// A boolean image option and the value it sets.
    Flag(PeFlag, bool),
    Subsystem,
    SectionAlignment,
    FileAlignment,
    Stack,
    Heap,
    MajorImageVersion,
    MinorImageVersion,
    MajorOsVersion,
    MinorOsVersion,
    MajorSubsystemVersion,
    MinorSubsystemVersion,
    OutImplib,
    OutputDef,
    ExcludeSymbols,
    ExcludeModulesForImplib,
    Export,
}

/// A boolean PE/COFF option, set by an `--x` / `--disable-x` pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeFlag {
    Dynamicbase,
    Nxcompat,
    HighEntropyVa,
    Tsaware,
    NoSeh,
    ForceInteg,
    NoIsolation,
    NoBind,
    WdmDriver,
    LargeAddressAware,
    RelocSection,
    InsertTimestamp,
    ExportAllSymbols,
    ExcludeAllSymbols,
    KillAt,
    AddStdcallAlias,
    StdcallFixup,
    AutoImport,
    RuntimePseudoReloc,
    WarnDuplicateExports,
}

/// What an implemented `-z` keyword does. Private to the parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ZAction {
    None,
    Now(bool),
    Relro(bool),
    Defs(bool),
    Muldefs,
    ExecStack(crate::args::options::ExecStack),
    GnuStack(bool),
    SeparateCode(SeparateCode),
    MaxPageSize,
    CommonPageSize,
    StackSize,
    CopyReloc(bool),
    CombReloc(bool),
    PackRelativeRelocs(bool),
    Text(bool),
    DynFlag(DynFlag),
    StartStopGc(bool),
    StartStopVisibility,
    KeepTextSectionPrefix(bool),
    Ibt,
    Shstk,
    IbtPlt,
    CetReport,
    IsaLevel(u8),
    DynamicUndefinedWeak(bool),
    ExternProtectedData(bool),
    MarkPlt(bool),
    SectionHeader(bool),
    MemorySeal(bool),
    DeadRelocInNonalloc,
}

/// A `DT_FLAGS_1` bit set by a `-z` keyword.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DynFlag {
    Nodelete,
    Nodlopen,
    Nodump,
    Initfirst,
    Interpose,
    Global,
    Nodefaultlib,
    Loadfltr,
    Origin,
    Singleton(bool),
}

const F: ArgKind = ArgKind::Flag;
const V: ArgKind = ArgKind::Value;
const OV: ArgKind = ArgKind::OptionalValue;
const EV: ArgKind = ArgKind::EqualsValue;
const JV: ArgKind = ArgKind::JoinedValue;

const fn imp(
    name: &'static str,
    arg: ArgKind,
    action: Action,
    meta: &'static str,
    help: &'static str,
) -> OptionDef {
    OptionDef {
        name,
        arg,
        status: Status::Implemented,
        meta,
        help,
        action,
    }
}

/// An implemented alias of an entry that carries the help text.
const fn alias(name: &'static str, arg: ArgKind, action: Action) -> OptionDef {
    imp(name, arg, action, "", "")
}

const fn ign(name: &'static str, arg: ArgKind) -> OptionDef {
    OptionDef {
        name,
        arg,
        status: Status::Ignored,
        meta: "",
        help: "",
        action: Action::None,
    }
}

const fn uns(name: &'static str, arg: ArgKind, why: &'static str) -> OptionDef {
    OptionDef {
        name,
        arg,
        status: Status::Unsupported(why),
        meta: "",
        help: "",
        action: Action::None,
    }
}

const M3: &str = "not implemented yet (roadmap M3: linker scripts and layout)";
const M4_ARM: &str = "not implemented yet (roadmap M4: Arm/AArch64 targets)";
const M4_OTHER: &str = "not implemented yet (roadmap M4: more ELF architectures)";
const M5: &str = "not implemented yet (roadmap M5: performance and optimization)";
const LTO: &str = "qld does LTO through a -plugin only (roadmap M6)";
const PE_V1: &str = "qld emits version 2 runtime pseudo-relocations only";
const PE_BASE_FILE: &str = "writing a base file for dlltool is not supported";
const PE_SEARCH_PREFIX: &str = "qld does not search for DLLs by name prefix";
const PE_LONG_NAMES: &str =
    "qld writes long section names whenever the output keeps a symbol table";
const PE_OLD_CODE: &str = "linking against pre-2000 MS import libraries is not supported";
const PE_COMPAT_IMPLIB: &str = "import libraries with undecorated aliases are not supported";
const PE_UNDERSCORE: &str = "the symbol prefix follows the target, and x86-64 PE has none";
const PE_DEFAULT_BASE: &str = "use --image-base to choose the image base";
const PE_PDB: &str = "PDB debug information is not supported";
const PE_DELAYLOAD: &str = "delay-loaded imports are not supported yet";
const PE_XLINK: &str = "passing options through to link.exe is not supported";
const PE_GUARD: &str = "Control Flow Guard is not supported";
const PE_LOAD_FLAG: &str = "the dependent load flag is not supported";
const PE_APPCONTAINER: &str = "AppContainer images are not supported";
const PE_FUNCTIONPADMIN: &str = "hot-patchable prologue padding is not supported";
const PE_EXE_SUFFIX: &str = "qld writes the output file it was given, with no added suffix";
const INCREMENTAL: &str = "incremental linking is not supported";
const NOT_PLANNED: &str = "not supported by qld";
const PRINTS: &str = "printing information and exiting is not supported yet";
const REPORT: &str = "writing this report is not supported yet";
const DYNAMIC: &str = "not supported by qld's dynamic linking yet";

use Action as A;
use PeAction as P;

/// Every GNU-flavor option qld recognizes.
pub static GNU_OPTIONS: &[OptionDef] = &[
    // ---- Help and version ----
    imp("help", F, A::Help, "", "Print this help and exit"),
    alias("target-help", F, A::Help),
    imp("version", F, A::Version, "", "Print the version and exit"),
    alias("v", F, A::Version),
    alias("V", F, A::Version),
    imp(
        "rsp-quoting",
        V,
        A::RspQuoting,
        "STYLE",
        "Quoting rules for @response files: posix or windows",
    ),
    // ---- Inputs and search paths ----
    imp(
        "l",
        V,
        A::Library,
        "LIBNAME",
        "Search for library LIBNAME (-l:FILE searches for FILE exactly)",
    ),
    alias("library", V, A::Library),
    imp(
        "L",
        V,
        A::LibraryPath,
        "DIR",
        "Add DIR to the library search path",
    ),
    alias("library-path", V, A::LibraryPath),
    alias("Y", V, A::LibraryPath),
    imp("T", V, A::Script, "FILE", "Read a linker script"),
    alias("script", V, A::Script),
    imp(
        "default-script",
        V,
        A::DefaultScript,
        "FILE",
        "Linker script to use when no -T script is given",
    ),
    alias("dT", V, A::DefaultScript),
    imp(
        "R",
        V,
        A::JustSymbols,
        "FILE",
        "Use only the symbol values from FILE",
    ),
    alias("just-symbols", V, A::JustSymbols),
    imp(
        "sysroot",
        V,
        A::Sysroot,
        "DIR",
        "Root for paths that start with = or $SYSROOT",
    ),
    imp(
        "nostdlib",
        F,
        A::Nostdlib,
        "",
        "Ignore SEARCH_DIR commands in linker scripts",
    ),
    // ---- Positional state ----
    imp(
        "whole-archive",
        F,
        A::WholeArchive(true),
        "",
        "Include every member of the archives that follow",
    ),
    imp(
        "no-whole-archive",
        F,
        A::WholeArchive(false),
        "",
        "Turn off --whole-archive",
    ),
    imp(
        "as-needed",
        F,
        A::AsNeeded(true),
        "",
        "Add DT_NEEDED for the shared libraries that follow only if used",
    ),
    imp(
        "no-as-needed",
        F,
        A::AsNeeded(false),
        "",
        "Turn off --as-needed",
    ),
    imp(
        "Bstatic",
        F,
        A::Static(true),
        "",
        "Do not link against shared libraries for the inputs that follow",
    ),
    alias("static", F, A::Static(true)),
    alias("dn", F, A::Static(true)),
    alias("non_shared", F, A::Static(true)),
    imp(
        "Bdynamic",
        F,
        A::Static(false),
        "",
        "Allow shared libraries again (default)",
    ),
    alias("dy", F, A::Static(false)),
    alias("call_shared", F, A::Static(false)),
    imp(
        "a",
        V,
        A::HpuxLinkMode,
        "KEYWORD",
        "HP-UX style: archive (= -Bstatic), shared or default (= -Bdynamic)",
    ),
    imp(
        "start-group",
        F,
        A::StartGroup,
        "",
        "Start a group of archives (accepted; qld resolves archives in any order)",
    ),
    alias("(", F, A::StartGroup),
    imp("end-group", F, A::EndGroup, "", "End a group"),
    alias(")", F, A::EndGroup),
    imp(
        "start-lib",
        F,
        A::StartLib,
        "",
        "Treat the object files that follow as archive members",
    ),
    imp("end-lib", F, A::EndLib, "", "End --start-lib"),
    imp(
        "push-state",
        F,
        A::PushState,
        "",
        "Save --whole-archive, --as-needed, -Bstatic and --format state",
    ),
    imp(
        "pop-state",
        F,
        A::PopState,
        "",
        "Restore the state saved by --push-state",
    ),
    imp(
        "copy-dt-needed-entries",
        F,
        A::CopyDtNeeded(true),
        "",
        "Follow DT_NEEDED entries of the shared libraries that follow",
    ),
    alias("add-needed", F, A::CopyDtNeeded(true)),
    imp(
        "no-copy-dt-needed-entries",
        F,
        A::CopyDtNeeded(false),
        "",
        "Turn off --copy-dt-needed-entries (default)",
    ),
    alias("no-add-needed", F, A::CopyDtNeeded(false)),
    imp(
        "format",
        V,
        A::Format,
        "FORMAT",
        "Format of the inputs that follow: default or binary",
    ),
    alias("b", V, A::Format),
    // ---- Output ----
    imp("o", V, A::Output, "FILE", "Set the output file name"),
    alias("output", V, A::Output),
    imp(
        "m",
        V,
        A::Emulation,
        "EMULATION",
        "Set the target (elf_x86_64, aarch64linux, i386pep, ...)",
    ),
    imp(
        "EB",
        F,
        A::Endian(Endianness::Big),
        "",
        "Link big-endian objects",
    ),
    imp(
        "EL",
        F,
        A::Endian(Endianness::Little),
        "",
        "Link little-endian objects",
    ),
    imp(
        "oformat",
        V,
        A::OutputFormat,
        "FORMAT",
        "Output format (binary, elf64-x86-64, ...)",
    ),
    imp("shared", F, A::Shared, "", "Create a shared library"),
    alias("Bshareable", F, A::Shared),
    imp(
        "pie",
        F,
        A::Pie(true),
        "",
        "Create a position-independent executable",
    ),
    alias("pic-executable", F, A::Pie(true)),
    imp(
        "no-pie",
        F,
        A::Pie(false),
        "",
        "Create a position-dependent executable (default)",
    ),
    imp("r", F, A::Relocatable, "", "Create relocatable output"),
    alias("relocatable", F, A::Relocatable),
    alias("i", F, A::Relocatable),
    uns(
        "Ur",
        F,
        "relocatable output with constructor tables is not supported",
    ),
    imp(
        "dynamic-linker",
        V,
        A::DynamicLinker,
        "PATH",
        "Set the program interpreter",
    ),
    alias("I", V, A::DynamicLinker),
    imp(
        "no-dynamic-linker",
        F,
        A::NoDynamicLinker,
        "",
        "Do not add a program interpreter",
    ),
    imp("e", V, A::Entry, "SYMBOL", "Set the entry point"),
    alias("entry", V, A::Entry),
    imp(
        "soname",
        V,
        A::Soname,
        "NAME",
        "Set the shared library name (DT_SONAME)",
    ),
    alias("h", V, A::Soname),
    imp(
        "rpath",
        V,
        A::Rpath,
        "DIR",
        "Add a run-time library search path",
    ),
    imp(
        "rpath-link",
        V,
        A::RpathLink,
        "DIR",
        "Add a link-time search path for shared library dependencies",
    ),
    imp(
        "enable-new-dtags",
        F,
        A::NewDtags(true),
        "",
        "Record -rpath as DT_RUNPATH",
    ),
    imp(
        "disable-new-dtags",
        F,
        A::NewDtags(false),
        "",
        "Record -rpath as DT_RPATH",
    ),
    imp(
        "init",
        V,
        A::Init,
        "SYMBOL",
        "Call SYMBOL at load time (DT_INIT)",
    ),
    imp(
        "fini",
        V,
        A::Fini,
        "SYMBOL",
        "Call SYMBOL at unload time (DT_FINI)",
    ),
    imp(
        "auxiliary",
        V,
        A::Auxiliary,
        "NAME",
        "Add an auxiliary filter (DT_AUXILIARY)",
    ),
    alias("f", V, A::Auxiliary),
    imp(
        "filter",
        V,
        A::Filter,
        "NAME",
        "Add a standard filter (DT_FILTER)",
    ),
    alias("F", V, A::Filter),
    imp(
        "spare-dynamic-tags",
        V,
        A::SpareDynamicTags,
        "COUNT",
        "Reserve COUNT extra .dynamic entries",
    ),
    // ---- Symbols ----
    imp(
        "u",
        V,
        A::Undefined,
        "SYMBOL",
        "Start with an undefined reference to SYMBOL",
    ),
    alias("undefined", V, A::Undefined),
    imp(
        "undefined-glob",
        V,
        A::UndefinedGlob,
        "PATTERN",
        "Add undefined references to archive symbols matching PATTERN",
    ),
    imp(
        "require-defined",
        V,
        A::RequireDefined,
        "SYMBOL",
        "Require SYMBOL to be defined",
    ),
    imp("defsym", V, A::Defsym, "SYMBOL=EXPR", "Define a symbol"),
    imp(
        "wrap",
        V,
        A::Wrap,
        "SYMBOL",
        "Redirect references to SYMBOL to __wrap_SYMBOL",
    ),
    imp(
        "y",
        V,
        A::TraceSymbol,
        "SYMBOL",
        "Report every file that mentions SYMBOL",
    ),
    alias("trace-symbol", V, A::TraceSymbol),
    imp(
        "export-dynamic",
        F,
        A::ExportDynamic(true),
        "",
        "Export all symbols to the dynamic symbol table",
    ),
    alias("E", F, A::ExportDynamic(true)),
    imp(
        "no-export-dynamic",
        F,
        A::ExportDynamic(false),
        "",
        "Turn off --export-dynamic (default)",
    ),
    imp(
        "export-dynamic-symbol",
        V,
        A::ExportDynamicSymbol,
        "GLOB",
        "Export symbols matching GLOB",
    ),
    imp(
        "export-dynamic-symbol-list",
        V,
        A::ExportDynamicSymbolList,
        "FILE",
        "Export symbols matching the patterns in FILE",
    ),
    imp(
        "dynamic-list",
        V,
        A::DynamicList,
        "FILE",
        "Export the symbols listed in FILE",
    ),
    uns("dynamic-list-data", F, DYNAMIC),
    uns("dynamic-list-cpp-new", F, DYNAMIC),
    uns("dynamic-list-cpp-typeinfo", F, DYNAMIC),
    imp(
        "exclude-libs",
        V,
        A::ExcludeLibs,
        "LIBS",
        "Hide the symbols of the listed archives (comma-separated, or ALL)",
    ),
    imp(
        "version-script",
        V,
        A::VersionScript,
        "FILE",
        "Read a symbol version script",
    ),
    uns("version-exports-section", V, DYNAMIC),
    imp(
        "undefined-version",
        F,
        A::UndefinedVersion(true),
        "",
        "Allow version scripts to name undefined symbols",
    ),
    imp(
        "no-undefined-version",
        F,
        A::UndefinedVersion(false),
        "",
        "Reject version scripts that name undefined symbols",
    ),
    imp(
        "default-symver",
        F,
        A::DefaultSymver,
        "",
        "Version exported symbols with the soname",
    ),
    uns("default-imported-symver", F, DYNAMIC),
    imp(
        "Bsymbolic",
        F,
        A::Symbolic(SymbolicMode::All),
        "",
        "Bind definitions locally in a shared library",
    ),
    imp(
        "Bsymbolic-functions",
        F,
        A::Symbolic(SymbolicMode::Functions),
        "",
        "Bind function definitions locally in a shared library",
    ),
    imp(
        "Bsymbolic-non-weak",
        F,
        A::Symbolic(SymbolicMode::NonWeak),
        "",
        "Bind non-weak definitions locally in a shared library",
    ),
    imp(
        "Bsymbolic-non-weak-functions",
        F,
        A::Symbolic(SymbolicMode::NonWeakFunctions),
        "",
        "Bind non-weak function definitions locally in a shared library",
    ),
    imp(
        "Bno-symbolic",
        F,
        A::Symbolic(SymbolicMode::None),
        "",
        "Do not bind definitions locally (default)",
    ),
    uns("Bgroup", F, DYNAMIC),
    imp(
        "no-undefined",
        F,
        A::NoUndefined,
        "",
        "Report undefined symbols even when creating a shared library",
    ),
    imp(
        "allow-shlib-undefined",
        F,
        A::AllowShlibUndefined(true),
        "",
        "Allow undefined symbols in shared library inputs",
    ),
    imp(
        "no-allow-shlib-undefined",
        F,
        A::AllowShlibUndefined(false),
        "",
        "Report undefined symbols in shared library inputs",
    ),
    imp(
        "unresolved-symbols",
        V,
        A::UnresolvedSymbols,
        "METHOD",
        "ignore-all, report-all, ignore-in-object-files or ignore-in-shared-libs",
    ),
    imp(
        "warn-unresolved-symbols",
        F,
        A::WarnUnresolvedSymbols(true),
        "",
        "Report unresolved symbols as warnings",
    ),
    imp(
        "error-unresolved-symbols",
        F,
        A::WarnUnresolvedSymbols(false),
        "",
        "Report unresolved symbols as errors (default)",
    ),
    imp(
        "ignore-unresolved-symbol",
        V,
        A::IgnoreUnresolvedSymbol,
        "SYMBOL",
        "Do not report SYMBOL if it is unresolved",
    ),
    imp(
        "allow-multiple-definition",
        F,
        A::AllowMultipleDefinition(true),
        "",
        "Allow multiple definitions; the first one wins",
    ),
    imp(
        "no-allow-multiple-definition",
        F,
        A::AllowMultipleDefinition(false),
        "",
        "Report multiple definitions (default)",
    ),
    imp(
        "d",
        F,
        A::DefineCommon,
        "",
        "Allocate common symbols even in relocatable output",
    ),
    alias("dc", F, A::DefineCommon),
    alias("dp", F, A::DefineCommon),
    uns("no-define-common", F, NOT_PLANNED),
    imp(
        "retain-symbols-file",
        V,
        A::RetainSymbolsFile,
        "FILE",
        "Keep only the symbols listed in FILE",
    ),
    uns("weak-unresolved-symbols", F, NOT_PLANNED),
    ign("disable-multiple-abs-defs", F),
    // ---- Sections and layout ----
    imp(
        "gc-sections",
        F,
        A::GcSections(true),
        "",
        "Remove unreferenced sections",
    ),
    imp(
        "no-gc-sections",
        F,
        A::GcSections(false),
        "",
        "Keep unreferenced sections (default)",
    ),
    imp(
        "print-gc-sections",
        F,
        A::PrintGcSections(true),
        "",
        "List the sections removed by --gc-sections",
    ),
    imp(
        "no-print-gc-sections",
        F,
        A::PrintGcSections(false),
        "",
        "Do not list removed sections (default)",
    ),
    imp(
        "gc-keep-exported",
        F,
        A::GcKeepExported,
        "",
        "Treat exported symbols as --gc-sections roots",
    ),
    imp(
        "why-live",
        V,
        A::WhyLive,
        "SYMBOL",
        "Explain why SYMBOL survives --gc-sections",
    ),
    imp(
        "icf",
        V,
        A::Icf,
        "MODE",
        "Identical code folding: none, safe or all",
    ),
    ign("icf-iterations", V),
    imp(
        "print-icf-sections",
        F,
        A::PrintIcfSections(true),
        "",
        "List the sections folded by --icf",
    ),
    imp(
        "no-print-icf-sections",
        F,
        A::PrintIcfSections(false),
        "",
        "Do not list folded sections (default)",
    ),
    imp(
        "keep-unique",
        V,
        A::KeepUnique,
        "SYMBOL",
        "Never fold SYMBOL with --icf",
    ),
    imp(
        "ignore-data-address-equality",
        F,
        A::IgnoreDataAddressEquality,
        "",
        "Allow folding data whose address is taken",
    ),
    imp(
        "ignore-function-address-equality",
        F,
        A::IgnoreFunctionAddressEquality,
        "",
        "Allow folding functions whose address is taken",
    ),
    imp("s", F, A::Strip(StripMode::All), "", "Strip all symbols"),
    alias("strip-all", F, A::Strip(StripMode::All)),
    imp(
        "S",
        F,
        A::Strip(StripMode::Debug),
        "",
        "Strip debug information",
    ),
    alias("strip-debug", F, A::Strip(StripMode::Debug)),
    uns("strip-debug-non-line", F, NOT_PLANNED),
    uns("strip-debug-gdb", F, NOT_PLANNED),
    ign("strip-lto-sections", F),
    ign("strip-discarded", F),
    uns("no-strip-discarded", F, NOT_PLANNED),
    imp(
        "x",
        F,
        A::Discard(DiscardMode::All),
        "",
        "Discard all local symbols",
    ),
    alias("discard-all", F, A::Discard(DiscardMode::All)),
    imp(
        "X",
        F,
        A::Discard(DiscardMode::Locals),
        "",
        "Discard temporary local symbols",
    ),
    alias("discard-locals", F, A::Discard(DiscardMode::Locals)),
    imp(
        "discard-none",
        F,
        A::Discard(DiscardMode::None),
        "",
        "Keep all local symbols",
    ),
    imp("q", F, A::EmitRelocs, "", "Keep relocations in the output"),
    alias("emit-relocs", F, A::EmitRelocs),
    imp(
        "n",
        F,
        A::Magic(MagicMode::Nmagic),
        "",
        "Do not page-align sections",
    ),
    alias("nmagic", F, A::Magic(MagicMode::Nmagic)),
    imp(
        "N",
        F,
        A::Magic(MagicMode::Omagic),
        "",
        "Do not page-align sections, and make text writable",
    ),
    alias("omagic", F, A::Magic(MagicMode::Omagic)),
    imp(
        "no-omagic",
        F,
        A::Magic(MagicMode::Normal),
        "",
        "Page-align sections (default)",
    ),
    alias("no-nmagic", F, A::Magic(MagicMode::Normal)),
    imp(
        "relax",
        F,
        A::Relax(true),
        "",
        "Apply target-specific relaxations (default)",
    ),
    imp(
        "no-relax",
        F,
        A::Relax(false),
        "",
        "Do not apply relaxations",
    ),
    imp(
        "image-base",
        V,
        A::ImageBase,
        "ADDRESS",
        "Set the base address",
    ),
    imp(
        "section-start",
        V,
        A::SectionStart,
        "SECTION=ADDRESS",
        "Set the address of a section (hexadecimal)",
    ),
    imp(
        "Ttext",
        V,
        A::SectionStartNamed(".text"),
        "ADDRESS",
        "Set the address of .text",
    ),
    imp(
        "Tdata",
        V,
        A::SectionStartNamed(".data"),
        "ADDRESS",
        "Set the address of .data",
    ),
    imp(
        "Tbss",
        V,
        A::SectionStartNamed(".bss"),
        "ADDRESS",
        "Set the address of .bss",
    ),
    imp(
        "Ttext-segment",
        V,
        A::TextSegment,
        "ADDRESS",
        "Set the address of the text segment",
    ),
    imp(
        "Trodata-segment",
        V,
        A::RodataSegment,
        "ADDRESS",
        "Set the address of the read-only data segment",
    ),
    imp(
        "Tldata-segment",
        V,
        A::LdataSegment,
        "ADDRESS",
        "Set the address of the large data segment",
    ),
    imp(
        "orphan-handling",
        V,
        A::OrphanHandling,
        "MODE",
        "Orphan sections: place, warn, error or discard",
    ),
    imp(
        "sort-section",
        V,
        A::SortSection,
        "KEY",
        "Sort wildcard section matches by name or alignment",
    ),
    imp(
        "rosegment",
        F,
        A::Rosegment(true),
        "",
        "Put read-only data in its own segment",
    ),
    imp(
        "no-rosegment",
        F,
        A::Rosegment(false),
        "",
        "Allow read-only data to share the code segment",
    ),
    imp(
        "eh-frame-hdr",
        F,
        A::EhFrameHdr(true),
        "",
        "Create .eh_frame_hdr and PT_GNU_EH_FRAME",
    ),
    imp(
        "no-eh-frame-hdr",
        F,
        A::EhFrameHdr(false),
        "",
        "Do not create .eh_frame_hdr",
    ),
    imp(
        "build-id",
        OV,
        A::BuildId,
        "STYLE",
        "Add a build ID note: fast, md5, sha1 (default), uuid, 0xHEX or none",
    ),
    imp(
        "hash-style",
        V,
        A::HashStyle,
        "STYLE",
        "Dynamic hash tables: sysv, gnu or both",
    ),
    imp(
        "compress-debug-sections",
        V,
        A::CompressDebugSections,
        "TYPE",
        "Compress debug sections: none, zlib, zlib-gnu, zlib-gabi or zstd",
    ),
    uns("compress-sections", V, M5),
    imp(
        "package-metadata",
        OV,
        A::PackageMetadata,
        "JSON",
        "Add a .note.package section",
    ),
    imp(
        "pack-dyn-relocs",
        V,
        A::PackDynRelocs,
        "FORMAT",
        "Pack relative relocations: none or relr",
    ),
    uns("use-android-relr-tags", F, NOT_PLANNED),
    ign("no-use-android-relr-tags", F),
    imp(
        "apply-dynamic-relocs",
        F,
        A::ApplyDynamicRelocs(true),
        "",
        "Also write the link-time value of dynamic relocations",
    ),
    imp(
        "no-apply-dynamic-relocs",
        F,
        A::ApplyDynamicRelocs(false),
        "",
        "Do not write dynamic relocation values (default)",
    ),
    imp(
        "fix-cortex-a53-843419",
        F,
        A::FixCortexA53Erratum843419,
        "",
        "Work around AArch64 Cortex-A53 erratum 843419",
    ),
    imp(
        "z",
        V,
        A::Z,
        "KEYWORD",
        "Set an ELF option (keywords listed below)",
    ),
    uns("unique", OV, M3),
    uns("section-ordering-file", V, M5),
    uns("symbol-ordering-file", V, M5),
    uns("call-graph-ordering-file", V, M5),
    ign("call-graph-profile-sort", OV),
    ign("no-call-graph-profile-sort", F),
    uns("gdb-index", F, M5),
    ign("no-gdb-index", F),
    uns("debug-names", F, M5),
    ign("no-debug-names", F),
    uns("separate-debug-file", OV, M5),
    uns("enable-non-contiguous-regions", F, M3),
    uns("enable-non-contiguous-regions-warnings", F, M3),
    uns("force-group-allocation", F, NOT_PLANNED),
    uns("split-by-file", OV, NOT_PLANNED),
    uns("split-by-reloc", OV, NOT_PLANNED),
    uns("remap-inputs", V, NOT_PLANNED),
    uns("remap-inputs-file", V, NOT_PLANNED),
    uns("shuffle-sections", OV, NOT_PLANNED),
    uns("reverse-sections", F, NOT_PLANNED),
    uns("randomize-section-padding", EV, NOT_PLANNED),
    uns("section-order", V, M5),
    uns("physical-image-base", V, M3),
    uns("spare-program-headers", V, NOT_PLANNED),
    uns("relocatable-merge-sections", F, NOT_PLANNED),
    uns("execute-only", F, M4_ARM),
    ign("no-execute-only", F),
    uns("optimize-bb-jumps", F, M5),
    ign("no-optimize-bb-jumps", F),
    ign("fortran-common", F),
    uns("no-fortran-common", F, NOT_PLANNED),
    ign("gnu-unique", F),
    uns("no-gnu-unique", F, NOT_PLANNED),
    uns("split-stack-adjust-size", V, NOT_PLANNED),
    ign("rosegment-gap", V),
    // ---- Architecture-specific ----
    uns("be8", F, M4_ARM),
    uns("fix-cortex-a8", F, M4_ARM),
    uns("fix-cortex-a53-835769", F, M4_ARM),
    uns("fix-arm1176", F, M4_ARM),
    uns("pic-veneer", F, M4_ARM),
    uns("long-plt", F, M4_ARM),
    ign("merge-exidx-entries", F),
    uns("no-merge-exidx-entries", F, M4_ARM),
    ign("target1-abs", F),
    uns("target1-rel", F, M4_ARM),
    uns("target2", V, M4_ARM),
    uns("cmse-implib", F, M4_ARM),
    uns("in-implib", V, M4_ARM),
    uns("stub-group-size", V, M4_OTHER),
    ign("secure-plt", F),
    ign("toc-optimize", F),
    ign("no-toc-optimize", F),
    ign("pcrel-optimize", F),
    ign("no-pcrel-optimize", F),
    ign("power10-stubs", OV),
    ign("no-power10-stubs", F),
    ign("toc-sort", F),
    uns("plt-align", OV, M4_OTHER),
    uns("plt-localentry", F, M4_OTHER),
    uns("plt-static-chain", F, M4_OTHER),
    uns("plt-thread-safe", F, M4_OTHER),
    uns("relax-gp", F, M4_OTHER),
    ign("no-relax-gp", F),
    uns("mips-got-size", V, NOT_PLANNED),
    uns("embedded-relocs", F, NOT_PLANNED),
    uns("android-memtag-stack", F, NOT_PLANNED),
    uns("android-memtag-heap", F, NOT_PLANNED),
    uns("android-memtag-mode", V, NOT_PLANNED),
    ign("gnu2-tls-tag", F),
    ign("no-gnu2-tls-tag", F),
    ign("gnu-tls-tag", F),
    ign("no-gnu-tls-tag", F),
    // ---- Rarely used GNU ld options ----
    uns("audit", V, DYNAMIC),
    uns("depaudit", V, DYNAMIC),
    uns("P", V, DYNAMIC),
    uns("c", V, "MRI linker scripts are not supported"),
    uns("mri-script", V, "MRI linker scripts are not supported"),
    ign("A", V),
    ign("architecture", V),
    ign("G", V),
    ign("gpsize", V),
    ign("g", F),
    ign("Qy", F),
    ign("qmagic", F),
    ign("assert", V),
    ign("accept-unknown-input-arch", F),
    ign("no-accept-unknown-input-arch", F),
    ign("check-sections", F),
    ign("no-check-sections", F),
    ign("no-keep-memory", F),
    ign("reduce-memory-overheads", F),
    ign("max-cache-size", V),
    ign("hash-size", V),
    ign("no-warn-mismatch", F),
    ign("no-warn-search-mismatch", F),
    ign("warn-mismatch", F),
    ign("warn-search-mismatch", F),
    ign("sort-common", OV),
    ign("stats", F),
    ign("no-stats", F),
    ign("print-memory-usage", F),
    ign("traditional-format", F),
    uns("task-link", V, NOT_PLANNED),
    uns("print-output-format", F, PRINTS),
    uns("print-sysroot", F, PRINTS),
    imp(
        "plugin-save-temps",
        F,
        Action::PluginSaveTemps,
        "",
        "Keep the files LTO plugins generate",
    ),
    ign("flto", OV),
    ign("flto-partition", V),
    ign("fuse-ld", V),
    ign("map-whole-files", F),
    ign("no-map-whole-files", F),
    ign("print-map-discarded", F),
    ign("no-print-map-discarded", F),
    ign("print-map-locals", F),
    ign("no-print-map-locals", F),
    ign("ctf-variables", F),
    ign("no-ctf-variables", F),
    ign("ctf-share-types", V),
    ign("disable-linker-version", F),
    ign("enable-linker-version", F),
    ign("ld-generated-unwind-info", F),
    ign("no-ld-generated-unwind-info", F),
    ign("discard-sframe", F),
    ign("error-handling-script", V),
    // ---- Diagnostics ----
    ign("warn-constructors", F),
    ign("warn-multiple-gp", F),
    ign("warn-once", F),
    ign("warn-section-align", F),
    ign("warn-alternate-em", F),
    ign("warn-execstack", F),
    ign("no-warn-execstack", F),
    ign("warn-execstack-objects", F),
    ign("error-execstack", F),
    ign("no-error-execstack", F),
    ign("warn-rwx-segments", F),
    ign("no-warn-rwx-segments", F),
    ign("error-rwx-segments", F),
    ign("no-error-rwx-segments", F),
    ign("warn-shared-textrel", F),
    ign("warn-ifunc-textrel", F),
    ign("no-warn-ifunc-textrel", F),
    ign("warn-symbol-ordering", F),
    ign("no-warn-symbol-ordering", F),
    ign("warn-drop-version", F),
    ign("no-wchar-size-warning", F),
    ign("no-enum-size-warning", F),
    ign("detect-odr-violations", F),
    ign("vs-diagnostics", F),
    imp(
        "warn-common",
        F,
        A::WarnCommon(true),
        "",
        "Warn about duplicate common symbols",
    ),
    imp(
        "no-warn-common",
        F,
        A::WarnCommon(false),
        "",
        "Do not warn about common symbols (default)",
    ),
    imp(
        "warn-backrefs",
        F,
        A::WarnBackrefs(true),
        "",
        "Warn about archive references GNU ld would not resolve",
    ),
    imp(
        "no-warn-backrefs",
        F,
        A::WarnBackrefs(false),
        "",
        "Turn off --warn-backrefs (default)",
    ),
    imp(
        "warn-backrefs-exclude",
        V,
        A::WarnBackrefsExclude,
        "GLOB",
        "Skip --warn-backrefs for archives matching GLOB",
    ),
    imp(
        "warn-textrel",
        F,
        A::WarnTextrel,
        "",
        "Warn if the output needs text relocations",
    ),
    imp(
        "fatal-warnings",
        F,
        A::FatalWarnings(true),
        "",
        "Treat warnings as errors",
    ),
    imp(
        "no-fatal-warnings",
        F,
        A::FatalWarnings(false),
        "",
        "Do not treat warnings as errors (default)",
    ),
    imp("w", F, A::NoWarnings, "", "Suppress warnings"),
    alias("no-warnings", F, A::NoWarnings),
    imp(
        "error-limit",
        V,
        A::ErrorLimit,
        "N",
        "Stop after N errors (0 means no limit)",
    ),
    imp(
        "color-diagnostics",
        OV,
        A::Color,
        "WHEN",
        "Color diagnostics: auto, always (default for the flag) or never",
    ),
    imp(
        "no-color-diagnostics",
        F,
        A::NoColor,
        "",
        "Do not color diagnostics",
    ),
    imp(
        "demangle",
        OV,
        A::Demangle(true),
        "STYLE",
        "Demangle symbol names in diagnostics (default)",
    ),
    imp(
        "no-demangle",
        F,
        A::Demangle(false),
        "",
        "Do not demangle symbol names",
    ),
    imp(
        "noinhibit-exec",
        F,
        A::NoinhibitExec,
        "",
        "Write the output even if errors occur",
    ),
    imp("Map", V, A::MapFile, "FILE", "Write a link map to FILE"),
    imp(
        "M",
        F,
        A::PrintMap,
        "",
        "Print a link map to standard output",
    ),
    alias("print-map", F, A::PrintMap),
    imp(
        "cref",
        F,
        A::Cref,
        "",
        "Add a cross-reference table to the link map",
    ),
    imp("t", F, A::Trace, "", "Print the name of each input file"),
    alias("trace", F, A::Trace),
    imp(
        "verbose",
        OV,
        A::Verbose,
        "LEVEL",
        "Print more information about the link",
    ),
    imp(
        "dependency-file",
        V,
        A::DependencyFile,
        "FILE",
        "Write a make-style dependency file",
    ),
    imp(
        "dependent-libraries",
        F,
        A::DependentLibraries(true),
        "",
        "Honor library names embedded in objects (default)",
    ),
    imp(
        "no-dependent-libraries",
        F,
        A::DependentLibraries(false),
        "",
        "Ignore library names embedded in objects",
    ),
    ign("time-trace", OV),
    ign("time-trace-granularity", V),
    uns("reproduce", V, REPORT),
    uns("repro", F, REPORT),
    uns("why-extract", EV, REPORT),
    uns("print-archive-stats", EV, REPORT),
    uns("print-symbol-order", V, REPORT),
    uns("print-symbol-counts", V, REPORT),
    uns("print-dependencies", F, REPORT),
    ign("check-dynamic-relocations", F),
    ign("no-check-dynamic-relocations", F),
    // ---- Threads and process behavior ----
    imp("O", V, A::Optimize, "LEVEL", "Optimization level"),
    imp(
        "threads",
        OV,
        A::Threads,
        "N",
        "Use N threads (default: one per core)",
    ),
    alias("thread-count", V, A::Threads),
    imp("no-threads", F, A::NoThreads, "", "Use a single thread"),
    ign("thread-count-initial", V),
    ign("thread-count-middle", V),
    ign("thread-count-final", V),
    ign("mmap-output-file", F),
    ign("no-mmap-output-file", F),
    ign("detach", F),
    ign("no-detach", F),
    imp(
        "fork",
        F,
        A::Fork(true),
        "",
        "Link in a child process; return once the output is written (default)",
    ),
    imp(
        "no-fork",
        F,
        A::Fork(false),
        "",
        "Link in the qld process itself",
    ),
    ign("perf", F),
    ign("quick-exit", F),
    ign("no-quick-exit", F),
    ign("keep-files-mapped", F),
    ign("no-keep-files-mapped", F),
    ign("posix-fallocate", F),
    ign("no-posix-fallocate", F),
    ign("preread-archive-symbols", F),
    ign("text-reorder", F),
    ign("no-text-reorder", F),
    ign("ctors-in-init-array", F),
    ign("no-ctors-in-init-array", F),
    ign("debug", V),
    ign("hash-bucket-empty-fraction", V),
    ign("build-id-chunk-size-for-treehash", V),
    ign("build-id-min-file-size-for-treehash", V),
    uns("chroot", V, NOT_PLANNED),
    uns("run", V, NOT_PLANNED),
    uns("incremental", F, INCREMENTAL),
    uns("no-incremental", F, INCREMENTAL),
    uns("incremental-full", F, INCREMENTAL),
    uns("incremental-update", F, INCREMENTAL),
    uns("incremental-changed", F, INCREMENTAL),
    uns("incremental-unchanged", F, INCREMENTAL),
    uns("incremental-unknown", F, INCREMENTAL),
    uns("incremental-startup-unchanged", F, INCREMENTAL),
    uns("incremental-base", V, INCREMENTAL),
    uns("incremental-patch", V, INCREMENTAL),
    // ---- LTO ----
    imp("plugin", V, A::Plugin, "PLUGIN", "Load an LTO plugin"),
    imp(
        "plugin-opt",
        V,
        A::PluginOpt,
        "OPTION",
        "Pass OPTION to the most recent -plugin",
    ),
    ign("lto", EV),
    ign("lto-O", JV),
    ign("lto-CGO", JV),
    ign("lto-partitions", EV),
    ign("lto-aa-pipeline", EV),
    ign("lto-newpm-passes", EV),
    ign("lto-debug-pass-manager", F),
    ign("lto-cs-profile-generate", F),
    ign("lto-cs-profile-file", EV),
    ign("lto-pgo-warn-mismatch", F),
    ign("no-lto-pgo-warn-mismatch", F),
    ign("lto-known-safe-vtables", V),
    ign("lto-obj-path", EV),
    ign("lto-sample-profile", EV),
    ign("lto-validate-all-vtables-have-type-infos", F),
    ign("no-lto-validate-all-vtables-have-type-infos", F),
    ign("lto-whole-program-visibility", F),
    ign("no-lto-whole-program-visibility", F),
    ign("lto-basic-block-sections", EV),
    ign("lto-basic-block-address-map", F),
    ign("no-lto-basic-block-address-map", F),
    ign("lto-unique-basic-block-section-names", F),
    ign("no-lto-unique-basic-block-section-names", F),
    ign("thinlto-cache-dir", EV),
    ign("thinlto-cache-policy", V),
    ign("thinlto-jobs", EV),
    ign("fat-lto-objects", F),
    ign("no-fat-lto-objects", F),
    ign("disable-verify", F),
    ign("mllvm", V),
    ign("opt-remarks-filename", V),
    ign("opt-remarks-passes", V),
    ign("opt-remarks-format", V),
    ign("opt-remarks-with-hotness", F),
    ign("opt-remarks-hotness-threshold", V),
    ign("save-temps", OV),
    ign("load-pass-plugin", V),
    uns("lto-emit-asm", F, LTO),
    uns("lto-emit-llvm", F, LTO),
    uns("thinlto-emit-imports-files", F, LTO),
    uns("thinlto-emit-index-files", F, LTO),
    uns("thinlto-index-only", OV, LTO),
    uns("thinlto-object-suffix-replace", EV, LTO),
    uns("thinlto-prefix-replace", EV, LTO),
    uns("thinlto-single-module", EV, LTO),
    // ---- PE/COFF (MinGW) ----
    imp(
        "subsystem",
        V,
        A::Pe(P::Subsystem),
        "NAME[,MAJOR[.MINOR]]",
        "Set the subsystem: console, windows, native, posix, efi-app, ... or a number",
    ),
    alias("dll", F, A::Shared),
    imp(
        "section-alignment",
        V,
        A::Pe(P::SectionAlignment),
        "SIZE",
        "Set the alignment of sections in memory",
    ),
    imp(
        "file-alignment",
        V,
        A::Pe(P::FileAlignment),
        "SIZE",
        "Set the alignment of sections in the file",
    ),
    imp(
        "stack",
        V,
        A::Pe(P::Stack),
        "RESERVE[,COMMIT]",
        "Set the stack reserve and commit sizes",
    ),
    imp(
        "heap",
        V,
        A::Pe(P::Heap),
        "RESERVE[,COMMIT]",
        "Set the default heap reserve and commit sizes",
    ),
    imp(
        "major-image-version",
        V,
        A::Pe(P::MajorImageVersion),
        "N",
        "Set the major version of the image",
    ),
    imp(
        "minor-image-version",
        V,
        A::Pe(P::MinorImageVersion),
        "N",
        "Set the minor version of the image",
    ),
    imp(
        "major-os-version",
        V,
        A::Pe(P::MajorOsVersion),
        "N",
        "Set the major required operating system version",
    ),
    imp(
        "minor-os-version",
        V,
        A::Pe(P::MinorOsVersion),
        "N",
        "Set the minor required operating system version",
    ),
    imp(
        "major-subsystem-version",
        V,
        A::Pe(P::MajorSubsystemVersion),
        "N",
        "Set the major required subsystem version",
    ),
    imp(
        "minor-subsystem-version",
        V,
        A::Pe(P::MinorSubsystemVersion),
        "N",
        "Set the minor required subsystem version",
    ),
    imp(
        "out-implib",
        V,
        A::Pe(P::OutImplib),
        "FILE",
        "Write an import library for the exported symbols",
    ),
    imp(
        "output-def",
        V,
        A::Pe(P::OutputDef),
        "FILE",
        "Write a .def file describing the exported symbols",
    ),
    imp(
        "export-all-symbols",
        F,
        A::Pe(P::Flag(PeFlag::ExportAllSymbols, true)),
        "",
        "Export every global symbol of a DLL",
    ),
    imp(
        "exclude-all-symbols",
        F,
        A::Pe(P::Flag(PeFlag::ExcludeAllSymbols, true)),
        "",
        "Export nothing automatically",
    ),
    imp(
        "exclude-symbols",
        V,
        A::Pe(P::ExcludeSymbols),
        "SYM,SYM,...",
        "Do not export these symbols with --export-all-symbols",
    ),
    imp(
        "exclude-modules-for-implib",
        V,
        A::Pe(P::ExcludeModulesForImplib),
        "MOD,MOD,...",
        "Leave these objects and archives out of the import library",
    ),
    imp(
        "export",
        V,
        A::Pe(P::Export),
        "SPEC",
        "Export NAME[=INTERNAL][,@ORDINAL][,DATA][,NONAME][,PRIVATE] (a qld extension)",
    ),
    imp(
        "kill-at",
        F,
        A::Pe(P::Flag(PeFlag::KillAt, true)),
        "",
        "Remove the @N suffix from exported stdcall names",
    ),
    imp(
        "add-stdcall-alias",
        F,
        A::Pe(P::Flag(PeFlag::AddStdcallAlias, true)),
        "",
        "Also export stdcall symbols without their @N suffix",
    ),
    imp(
        "enable-stdcall-fixup",
        F,
        A::Pe(P::Flag(PeFlag::StdcallFixup, true)),
        "",
        "Resolve _foo@8 against _foo, and the other way round",
    ),
    imp(
        "disable-stdcall-fixup",
        F,
        A::Pe(P::Flag(PeFlag::StdcallFixup, false)),
        "",
        "Report a stdcall symbol that only differs by its @N suffix",
    ),
    imp(
        "warn-duplicate-exports",
        F,
        A::Pe(P::Flag(PeFlag::WarnDuplicateExports, true)),
        "",
        "Warn about symbols exported more than once",
    ),
    imp(
        "enable-auto-import",
        F,
        A::Pe(P::Flag(PeFlag::AutoImport, true)),
        "",
        "Fix up direct references to imported data at run time (default)",
    ),
    imp(
        "disable-auto-import",
        F,
        A::Pe(P::Flag(PeFlag::AutoImport, false)),
        "",
        "Report a direct reference to imported data",
    ),
    imp(
        "enable-runtime-pseudo-reloc",
        F,
        A::Pe(P::Flag(PeFlag::RuntimePseudoReloc, true)),
        "",
        "Emit the pseudo-relocation list auto-import needs (default)",
    ),
    imp(
        "disable-runtime-pseudo-reloc",
        F,
        A::Pe(P::Flag(PeFlag::RuntimePseudoReloc, false)),
        "",
        "Do not emit a pseudo-relocation list",
    ),
    alias(
        "enable-runtime-pseudo-reloc-v2",
        F,
        A::Pe(P::Flag(PeFlag::RuntimePseudoReloc, true)),
    ),
    uns("enable-runtime-pseudo-reloc-v1", F, PE_V1),
    imp(
        "dynamicbase",
        F,
        A::Pe(P::Flag(PeFlag::Dynamicbase, true)),
        "",
        "Let the image be relocated at load time, and emit .reloc (default)",
    ),
    imp(
        "disable-dynamicbase",
        F,
        A::Pe(P::Flag(PeFlag::Dynamicbase, false)),
        "",
        "Fix the image at its preferred base address",
    ),
    alias(
        "no-dynamicbase",
        F,
        A::Pe(P::Flag(PeFlag::Dynamicbase, false)),
    ),
    imp(
        "nxcompat",
        F,
        A::Pe(P::Flag(PeFlag::Nxcompat, true)),
        "",
        "Mark the image as compatible with data execution prevention (default)",
    ),
    imp(
        "disable-nxcompat",
        F,
        A::Pe(P::Flag(PeFlag::Nxcompat, false)),
        "",
        "Turn off --nxcompat",
    ),
    imp(
        "high-entropy-va",
        F,
        A::Pe(P::Flag(PeFlag::HighEntropyVa, true)),
        "",
        "Mark the image as compatible with 64-bit address space layout randomization (default)",
    ),
    imp(
        "disable-high-entropy-va",
        F,
        A::Pe(P::Flag(PeFlag::HighEntropyVa, false)),
        "",
        "Turn off --high-entropy-va",
    ),
    imp(
        "large-address-aware",
        F,
        A::Pe(P::Flag(PeFlag::LargeAddressAware, true)),
        "",
        "Mark the image as able to use more than 2 GiB (default)",
    ),
    imp(
        "disable-large-address-aware",
        F,
        A::Pe(P::Flag(PeFlag::LargeAddressAware, false)),
        "",
        "Turn off --large-address-aware",
    ),
    imp(
        "tsaware",
        F,
        A::Pe(P::Flag(PeFlag::Tsaware, true)),
        "",
        "Mark the image as Terminal Server aware",
    ),
    imp(
        "disable-tsaware",
        F,
        A::Pe(P::Flag(PeFlag::Tsaware, false)),
        "",
        "Turn off --tsaware (default)",
    ),
    imp(
        "no-seh",
        F,
        A::Pe(P::Flag(PeFlag::NoSeh, true)),
        "",
        "Mark the image as using no structured exception handlers",
    ),
    imp(
        "disable-no-seh",
        F,
        A::Pe(P::Flag(PeFlag::NoSeh, false)),
        "",
        "Turn off --no-seh (default)",
    ),
    imp(
        "forceinteg",
        F,
        A::Pe(P::Flag(PeFlag::ForceInteg, true)),
        "",
        "Ask the loader to check the image's signature",
    ),
    imp(
        "disable-forceinteg",
        F,
        A::Pe(P::Flag(PeFlag::ForceInteg, false)),
        "",
        "Turn off --forceinteg (default)",
    ),
    imp(
        "no-isolation",
        F,
        A::Pe(P::Flag(PeFlag::NoIsolation, true)),
        "",
        "Mark the image so that it is not isolated by a manifest",
    ),
    imp(
        "disable-no-isolation",
        F,
        A::Pe(P::Flag(PeFlag::NoIsolation, false)),
        "",
        "Turn off --no-isolation (default)",
    ),
    imp(
        "no-bind",
        F,
        A::Pe(P::Flag(PeFlag::NoBind, true)),
        "",
        "Mark the image as one that must not be bound",
    ),
    imp(
        "disable-no-bind",
        F,
        A::Pe(P::Flag(PeFlag::NoBind, false)),
        "",
        "Turn off --no-bind (default)",
    ),
    imp(
        "wdmdriver",
        F,
        A::Pe(P::Flag(PeFlag::WdmDriver, true)),
        "",
        "Mark the image as a WDM device driver",
    ),
    imp(
        "disable-wdmdriver",
        F,
        A::Pe(P::Flag(PeFlag::WdmDriver, false)),
        "",
        "Turn off --wdmdriver (default)",
    ),
    imp(
        "enable-reloc-section",
        F,
        A::Pe(P::Flag(PeFlag::RelocSection, true)),
        "",
        "Emit .reloc when the image is relocatable (default)",
    ),
    imp(
        "disable-reloc-section",
        F,
        A::Pe(P::Flag(PeFlag::RelocSection, false)),
        "",
        "Never emit a .reloc section",
    ),
    imp(
        "insert-timestamp",
        F,
        A::Pe(P::Flag(PeFlag::InsertTimestamp, true)),
        "",
        "Stamp the image with the current time",
    ),
    imp(
        "no-insert-timestamp",
        F,
        A::Pe(P::Flag(PeFlag::InsertTimestamp, false)),
        "",
        "Use a zero timestamp, so the output is reproducible (default)",
    ),
    ign("enable-auto-image-base", OV),
    ign("disable-auto-image-base", F),
    ign("enable-long-section-names", F),
    ign("enable-extra-pe-debug", F),
    ign("full-shutdown", F),
    uns("base-file", V, PE_BASE_FILE),
    uns("dll-search-prefix", V, PE_SEARCH_PREFIX),
    uns("disable-long-section-names", F, PE_LONG_NAMES),
    uns("support-old-code", F, PE_OLD_CODE),
    uns("thumb-entry", V, M4_ARM),
    uns("compat-implib", F, PE_COMPAT_IMPLIB),
    uns("leading-underscore", F, PE_UNDERSCORE),
    uns("no-leading-underscore", F, PE_UNDERSCORE),
    uns("default-image-base-low", F, PE_DEFAULT_BASE),
    uns("default-image-base-high", F, PE_DEFAULT_BASE),
    uns("pdb", V, PE_PDB),
    uns("delayload", V, PE_DELAYLOAD),
    uns("Xlink", V, PE_XLINK),
    uns("guard-cf", F, PE_GUARD),
    uns("no-guard-cf", F, PE_GUARD),
    uns("guard-longjmp", F, PE_GUARD),
    uns("no-guard-longjmp", F, PE_GUARD),
    uns("dependent-load-flag", V, PE_LOAD_FLAG),
    uns("appcontainer", F, PE_APPCONTAINER),
    uns("functionpadmin", OV, PE_FUNCTIONPADMIN),
    uns("force-exe-suffix", F, PE_EXE_SUFFIX),
];

const ZF: ZArg = ZArg::Flag;
const ZV: ZArg = ZArg::Value;

const fn zimp(name: &'static str, arg: ZArg, action: ZAction, help: &'static str) -> ZKeyword {
    ZKeyword {
        name,
        arg,
        status: Status::Implemented,
        help,
        action,
    }
}

const fn zign(name: &'static str, arg: ZArg) -> ZKeyword {
    ZKeyword {
        name,
        arg,
        status: Status::Ignored,
        help: "",
        action: ZAction::None,
    }
}

const fn zuns(name: &'static str, arg: ZArg, why: &'static str) -> ZKeyword {
    ZKeyword {
        name,
        arg,
        status: Status::Unsupported(why),
        help: "",
        action: ZAction::None,
    }
}

use crate::args::options::ExecStack;
use ZAction as Z;

/// Every `-z` keyword qld recognizes. Unknown keywords produce a warning.
pub static Z_KEYWORDS: &[ZKeyword] = &[
    zimp("now", ZF, Z::Now(true), "Resolve all symbols at load time"),
    zimp(
        "lazy",
        ZF,
        Z::Now(false),
        "Resolve symbols on first use (default)",
    ),
    zimp("relro", ZF, Z::Relro(true), "Create PT_GNU_RELRO (default)"),
    zimp("norelro", ZF, Z::Relro(false), "Do not create PT_GNU_RELRO"),
    zimp("defs", ZF, Z::Defs(true), "Same as --no-undefined"),
    zimp(
        "undefs",
        ZF,
        Z::Defs(false),
        "Allow undefined symbols in objects",
    ),
    zimp(
        "muldefs",
        ZF,
        Z::Muldefs,
        "Same as --allow-multiple-definition",
    ),
    zimp(
        "execstack",
        ZF,
        Z::ExecStack(ExecStack::Executable),
        "Mark the stack executable",
    ),
    zimp(
        "noexecstack",
        ZF,
        Z::ExecStack(ExecStack::NonExecutable),
        "Mark the stack non-executable",
    ),
    zimp(
        "execstack-if-needed",
        ZF,
        Z::ExecStack(ExecStack::FromInputs),
        "Mark the stack executable if an input needs it",
    ),
    zimp("nognustack", ZF, Z::GnuStack(false), "Omit PT_GNU_STACK"),
    zimp(
        "separate-code",
        ZF,
        Z::SeparateCode(SeparateCode::Code),
        "Put code in its own page-aligned segment",
    ),
    zimp(
        "noseparate-code",
        ZF,
        Z::SeparateCode(SeparateCode::None),
        "Let code share a segment with read-only data",
    ),
    zimp(
        "separate-loadable-segments",
        ZF,
        Z::SeparateCode(SeparateCode::Loadable),
        "Page-align every loadable segment",
    ),
    zimp(
        "max-page-size",
        ZV,
        Z::MaxPageSize,
        "Set the maximum page size",
    ),
    zimp(
        "common-page-size",
        ZV,
        Z::CommonPageSize,
        "Set the common page size",
    ),
    zimp("stack-size", ZV, Z::StackSize, "Set the PT_GNU_STACK size"),
    zimp(
        "copyreloc",
        ZF,
        Z::CopyReloc(true),
        "Allow copy relocations (default)",
    ),
    zimp(
        "nocopyreloc",
        ZF,
        Z::CopyReloc(false),
        "Do not create copy relocations",
    ),
    zimp(
        "combreloc",
        ZF,
        Z::CombReloc(true),
        "Sort and combine dynamic relocations (default)",
    ),
    zimp(
        "nocombreloc",
        ZF,
        Z::CombReloc(false),
        "Do not combine dynamic relocations",
    ),
    zimp(
        "pack-relative-relocs",
        ZF,
        Z::PackRelativeRelocs(true),
        "Pack relative relocations in DT_RELR",
    ),
    zimp(
        "nopack-relative-relocs",
        ZF,
        Z::PackRelativeRelocs(false),
        "Do not use DT_RELR (default)",
    ),
    zimp("text", ZF, Z::Text(true), "Make DT_TEXTREL an error"),
    zimp("notext", ZF, Z::Text(false), "Allow DT_TEXTREL (default)"),
    zimp("textoff", ZF, Z::Text(false), "Allow DT_TEXTREL"),
    zimp(
        "nodelete",
        ZF,
        Z::DynFlag(DynFlag::Nodelete),
        "Mark the library non-unloadable",
    ),
    zimp(
        "nodlopen",
        ZF,
        Z::DynFlag(DynFlag::Nodlopen),
        "Mark the library non-dlopen-able",
    ),
    zimp(
        "nodump",
        ZF,
        Z::DynFlag(DynFlag::Nodump),
        "Mark the object non-dldump-able",
    ),
    zimp(
        "initfirst",
        ZF,
        Z::DynFlag(DynFlag::Initfirst),
        "Initialize this object first",
    ),
    zimp(
        "interpose",
        ZF,
        Z::DynFlag(DynFlag::Interpose),
        "Interpose symbols of other objects",
    ),
    zimp(
        "global",
        ZF,
        Z::DynFlag(DynFlag::Global),
        "Make symbols globally available",
    ),
    zimp(
        "nodefaultlib",
        ZF,
        Z::DynFlag(DynFlag::Nodefaultlib),
        "Ignore default library search paths at run time",
    ),
    zimp(
        "loadfltr",
        ZF,
        Z::DynFlag(DynFlag::Loadfltr),
        "Load filtees immediately",
    ),
    zimp(
        "origin",
        ZF,
        Z::DynFlag(DynFlag::Origin),
        "Mark the object as using $ORIGIN",
    ),
    zimp(
        "unique",
        ZF,
        Z::DynFlag(DynFlag::Singleton(true)),
        "Load the object at most once (DF_1_SINGLETON)",
    ),
    zimp(
        "nounique",
        ZF,
        Z::DynFlag(DynFlag::Singleton(false)),
        "Turn off -z unique (default)",
    ),
    zimp(
        "start-stop-gc",
        ZF,
        Z::StartStopGc(true),
        "Let __start_/__stop_ references not retain sections",
    ),
    zimp(
        "nostart-stop-gc",
        ZF,
        Z::StartStopGc(false),
        "__start_/__stop_ references retain sections",
    ),
    zimp(
        "start-stop-visibility",
        ZV,
        Z::StartStopVisibility,
        "Visibility of __start_/__stop_ symbols",
    ),
    zimp(
        "keep-text-section-prefix",
        ZF,
        Z::KeepTextSectionPrefix(true),
        "Keep .text.hot, .text.unlikely, ... as separate output sections",
    ),
    zimp(
        "nokeep-text-section-prefix",
        ZF,
        Z::KeepTextSectionPrefix(false),
        "Merge .text.* into .text (default)",
    ),
    zimp("ibt", ZF, Z::Ibt, "Mark the output IBT-compatible"),
    zimp(
        "shstk",
        ZF,
        Z::Shstk,
        "Mark the output shadow-stack-compatible",
    ),
    zimp("ibtplt", ZF, Z::IbtPlt, "Generate IBT-enabled PLT entries"),
    zimp(
        "cet-report",
        ZV,
        Z::CetReport,
        "Report inputs lacking IBT/SHSTK: none, warning or error",
    ),
    zimp(
        "x86-64-baseline",
        ZF,
        Z::IsaLevel(1),
        "Mark x86-64 baseline ISA as needed",
    ),
    zimp(
        "x86-64-v2",
        ZF,
        Z::IsaLevel(2),
        "Mark x86-64-v2 ISA as needed",
    ),
    zimp(
        "x86-64-v3",
        ZF,
        Z::IsaLevel(3),
        "Mark x86-64-v3 ISA as needed",
    ),
    zimp(
        "x86-64-v4",
        ZF,
        Z::IsaLevel(4),
        "Mark x86-64-v4 ISA as needed",
    ),
    zimp(
        "dynamic-undefined-weak",
        ZF,
        Z::DynamicUndefinedWeak(true),
        "Make undefined weak symbols dynamic",
    ),
    zimp(
        "nodynamic-undefined-weak",
        ZF,
        Z::DynamicUndefinedWeak(false),
        "Do not make undefined weak symbols dynamic",
    ),
    zimp(
        "noextern-protected-data",
        ZF,
        Z::ExternProtectedData(false),
        "Do not treat protected data as external",
    ),
    zimp(
        "mark-plt",
        ZF,
        Z::MarkPlt(true),
        "Mark PLT entries with dynamic tags",
    ),
    zimp(
        "nomark-plt",
        ZF,
        Z::MarkPlt(false),
        "Do not mark PLT entries (default)",
    ),
    zimp(
        "sectionheader",
        ZF,
        Z::SectionHeader(true),
        "Write section headers (default)",
    ),
    zimp(
        "nosectionheader",
        ZF,
        Z::SectionHeader(false),
        "Omit section headers",
    ),
    zimp(
        "memory-seal",
        ZF,
        Z::MemorySeal(true),
        "Request memory sealing",
    ),
    zimp(
        "nomemory-seal",
        ZF,
        Z::MemorySeal(false),
        "Do not request memory sealing (default)",
    ),
    zign("nocommon", ZF),
    zign("noindirect-extern-access", ZF),
    zign("nounique-symbol", ZF),
    zign("rela", ZF),
    zign("noreloc-overflow", ZF),
    zign("isa-level-report", ZV),
    zign("report-relative-reloc", ZF),
    zign("lam-report", ZV),
    zign("lam-u48-report", ZV),
    zign("lam-u57-report", ZV),
    zign("nolrodata-after-bss", ZF),
    zign("bti-report", ZV),
    zign("gcs-report", ZV),
    zign("pauth-report", ZV),
    zign("execute-only-report", ZV),
    zign("zicfilp-unlabeled-report", ZV),
    zign("zicfilp-func-sig-report", ZV),
    zign("zicfiss-report", ZV),
    zign("call-nop", ZV),
    zign("norewrite-endbr", ZF),
    zuns("rel", ZF, NOT_PLANNED),
    zuns("rodynamic", ZF, NOT_PLANNED),
    zuns("wxneeded", ZF, NOT_PLANNED),
    zuns("lrodata-after-bss", ZF, NOT_PLANNED),
    zimp(
        "dead-reloc-in-nonalloc",
        ZV,
        Z::DeadRelocInNonalloc,
        "Value for relocations to discarded sections in non-alloc sections: <glob>=<value>",
    ),
    zuns("zicfilp", ZV, M4_OTHER),
    zuns("zicfiss", ZV, M4_OTHER),
    zuns("gcs", ZV, M4_ARM),
    zuns("force-bti", ZF, M4_ARM),
    zuns("pac-plt", ZF, M4_ARM),
    zuns("force-ibt", ZF, NOT_PLANNED),
    zuns("hazardplt", ZF, NOT_PLANNED),
    zuns("retpolineplt", ZF, NOT_PLANNED),
    zuns("ifunc-noplt", ZF, NOT_PLANNED),
    zuns("bndplt", ZF, NOT_PLANNED),
    zuns("common", ZF, NOT_PLANNED),
    zuns("indirect-extern-access", ZF, NOT_PLANNED),
    zuns("unique-symbol", ZF, NOT_PLANNED),
    zuns("globalaudit", ZF, DYNAMIC),
    zuns("lam-u48", ZF, NOT_PLANNED),
    zuns("lam-u57", ZF, NOT_PLANNED),
    zuns("rewrite-endbr", ZF, M5),
];

/// Looks up an option by its name without dashes.
#[must_use]
pub fn find(name: &str) -> Option<&'static OptionDef> {
    index().get(name.as_bytes()).copied()
}

/// Looks up a `-z` keyword by name, without any `=value`.
#[must_use]
pub fn find_z(name: &str) -> Option<&'static ZKeyword> {
    Z_KEYWORDS.iter().find(|keyword| keyword.name == name)
}

pub(crate) fn find_bytes(name: &[u8]) -> Option<&'static OptionDef> {
    index().get(name).copied()
}

/// Options whose value is joined directly to the name, for prefix matching.
pub(crate) fn joined_options() -> impl Iterator<Item = &'static OptionDef> {
    GNU_OPTIONS
        .iter()
        .filter(|def| def.arg == ArgKind::JoinedValue)
}

fn index() -> &'static std::collections::HashMap<&'static [u8], &'static OptionDef> {
    static INDEX: std::sync::OnceLock<
        std::collections::HashMap<&'static [u8], &'static OptionDef>,
    > = std::sync::OnceLock::new();
    INDEX.get_or_init(|| {
        GNU_OPTIONS
            .iter()
            .map(|def| (def.name.as_bytes(), def))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_names_are_unique() {
        let mut names: Vec<&str> = GNU_OPTIONS.iter().map(|def| def.name).collect();
        names.sort_unstable();
        for pair in names.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate option");
        }
        assert_eq!(index().len(), GNU_OPTIONS.len());
    }

    #[test]
    fn z_keywords_are_unique() {
        let mut names: Vec<&str> = Z_KEYWORDS.iter().map(|z| z.name).collect();
        names.sort_unstable();
        for pair in names.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate -z keyword");
        }
    }

    #[test]
    fn short_options_are_flags_or_values() {
        for def in GNU_OPTIONS.iter().filter(|def| def.name.len() == 1) {
            assert!(
                matches!(def.arg, ArgKind::Flag | ArgKind::Value),
                "-{} must be a flag or take a value",
                def.name
            );
        }
    }

    #[test]
    fn implemented_options_are_documented() {
        for def in GNU_OPTIONS {
            match def.status {
                Status::Implemented if def.help.is_empty() => {
                    assert!(
                        GNU_OPTIONS
                            .iter()
                            .any(|other| other.status == Status::Implemented
                                && !other.help.is_empty()
                                && other.action == def.action),
                        "alias {} has no documented primary option",
                        def.name
                    );
                }
                Status::Implemented => assert_ne!(def.action, Action::None, "{}", def.name),
                _ => assert!(def.help.is_empty(), "{} is not implemented", def.name),
            }
        }
        for z in Z_KEYWORDS {
            assert_eq!(
                z.status == Status::Implemented,
                !z.help.is_empty(),
                "-z {}",
                z.name
            );
        }
    }
}
