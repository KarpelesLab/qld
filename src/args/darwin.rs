//! The Apple ld64 command line.
//!
//! [`DARWIN_OPTIONS`] lists every ld64 option qld recognizes, with the same
//! [`Status`] policy as the GNU table (`docs/compatibility.md`): implemented
//! options fill [`DarwinArgs`] (and the flavor-neutral fields of
//! [`LinkOptions`]), ignored options have no effect on the output, and
//! unsupported ones are rejected by name.
//!
//! ld64 spelling is simpler than GNU's: every option has one dash, values
//! follow as separate arguments (`-arch arm64`, `-platform_version macos 13.0
//! 14.0`), and only the library and search path options take a joined value
//! (`-lSystem`, `-L/usr/lib`, `-weak-lfoo`). Anything not starting with `-`
//! is an input file, and `@file` expands a response file.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::args::options::{DiscardMode, Flavor, LinkOptions, StripMode};
use crate::args::parse::ParseOutcome;
use crate::args::response::{self, FileReader, Quoting};
use crate::args::table::Status;
use crate::error::{Error, Result};
use crate::macho::read::Arch;
use crate::macho::read::commands::PackedVersion;
use crate::macho::read::consts;
use crate::target::{BinaryFormat, Endianness, OperatingSystem, PointerWidth, Target};

/// What kind of Mach-O file the link produces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MachOutputType {
    /// `MH_EXECUTE` (`-execute`, the default).
    #[default]
    Execute,
    /// `MH_DYLIB` (`-dylib`).
    Dylib,
    /// `MH_BUNDLE` (`-bundle`).
    Bundle,
    /// `MH_OBJECT` (`-r`): a relocatable object.
    Object,
}

/// `-undefined <treatment>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UndefinedTreatment {
    /// `error` (default): undefined symbols fail the link.
    #[default]
    Error,
    /// `warning`: report them as warnings and look them up dynamically.
    Warning,
    /// `suppress`: look them up dynamically without a message.
    Suppress,
    /// `dynamic_lookup`: look them up dynamically (flat lookup at run time).
    DynamicLookup,
}

/// How `LC_UUID` is produced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UuidMode {
    /// A hash of the output (default, deterministic).
    #[default]
    Content,
    /// `-no_uuid`: no `LC_UUID`.
    None,
}

/// `-platform_version <platform> <min> <sdk>`, or the older
/// `-macos_version_min` family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformVersion {
    /// `PLATFORM_*` (`1` for macOS).
    pub platform: u32,
    /// Minimum deployment target.
    pub min: PackedVersion,
    /// SDK version.
    pub sdk: PackedVersion,
}

/// How a dylib or framework input is linked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LoadMode {
    /// A plain `LC_LOAD_DYLIB` (or an ordinary archive member search).
    #[default]
    Normal,
    /// `-weak-l`, `-weak_framework`, `-weak_library`: `LC_LOAD_WEAK_DYLIB`,
    /// and every import from the library is weak.
    Weak,
    /// `-reexport-l`, `-reexport_framework`, `-reexport_library`:
    /// `LC_REEXPORT_DYLIB`.
    Reexport,
    /// `-needed-l`, `-needed_framework`, `-needed_library`: kept even with
    /// `-dead_strip_dylibs`.
    Needed,
    /// `-hidden-l`: an archive whose symbols become private externs.
    Hidden,
}

/// What a Darwin input names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DarwinInputKind {
    /// A file path.
    File(PathBuf),
    /// `-l<name>`: `lib<name>.tbd`, `.dylib`, `.a` in the library search
    /// paths.
    Library(String),
    /// `-framework <name>[,<suffix>]`: `<name>.framework/<name>` in the
    /// framework search paths.
    Framework {
        /// The framework name.
        name: String,
        /// The optional suffix (`-framework Foo,_debug`).
        suffix: Option<String>,
    },
}

/// One Darwin input, in command-line order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DarwinInput {
    /// What the input names.
    pub kind: DarwinInputKind,
    /// How it is linked.
    pub mode: LoadMode,
    /// `-force_load`: load every member of this archive.
    pub force_load: bool,
}

/// The options of an ld64-flavor link that have no GNU equivalent.
///
/// The flavor-neutral parts of an ld64 command line go into the ordinary
/// [`LinkOptions`] fields: `-o` into `output`, `-e` into `entry`,
/// `-install_name` into `soname`, `-rpath` into `rpaths`, `-L` into
/// `search_paths`, `-dead_strip` into `gc_sections`, `-u` into `undefined`,
/// `-S` into `strip`, `-x` into `discard`, `-init` into `init`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DarwinArgs {
    /// `-arch` values, in order and without duplicates. Several make a
    /// universal binary. Empty means "infer from the first object".
    pub archs: Vec<Arch>,
    /// `-dylib`, `-bundle` or `-execute`.
    pub output_type: MachOutputType,
    /// `-platform_version` (or `-macos_version_min` and friends).
    pub platform: Option<PlatformVersion>,
    /// `-syslibroot` directories, in order.
    pub syslibroots: Vec<PathBuf>,
    /// `-F` framework search directories, in order.
    pub framework_paths: Vec<PathBuf>,
    /// `-Z`: do not search the default library and framework directories.
    pub no_default_search_paths: bool,
    /// `-search_dylibs_first`: search every directory for a dylib before
    /// looking for an archive (the default searches each directory for
    /// both).
    pub search_dylibs_first: bool,
    /// Inputs, in command-line order.
    pub inputs: Vec<DarwinInput>,
    /// `-all_load`: load every member of every archive.
    pub all_load: bool,
    /// `-ObjC`: load archive members that define Objective-C classes or
    /// categories.
    pub objc: bool,
    /// `-current_version`.
    pub current_version: Option<PackedVersion>,
    /// `-compatibility_version`.
    pub compatibility_version: Option<PackedVersion>,
    /// `-undefined`.
    pub undefined: UndefinedTreatment,
    /// `-U` symbols: allowed to stay undefined, looked up dynamically.
    pub dynamic_lookup_symbols: Vec<String>,
    /// `-exported_symbols_list` files.
    pub exported_symbols_lists: Vec<PathBuf>,
    /// `-unexported_symbols_list` files.
    pub unexported_symbols_lists: Vec<PathBuf>,
    /// `-exported_symbol` patterns.
    pub exported_symbols: Vec<String>,
    /// `-unexported_symbol` patterns.
    pub unexported_symbols: Vec<String>,
    /// `-no_exported_symbols`.
    pub no_exported_symbols: bool,
    /// `-order_file`.
    pub order_file: Option<PathBuf>,
    /// `-adhoc_codesign` (`Some(true)`) / `-no_adhoc_codesign`
    /// (`Some(false)`). `None` signs arm64 outputs only, like ld64.
    pub adhoc_codesign: Option<bool>,
    /// `-headerpad <size>`.
    pub headerpad: Option<u64>,
    /// `-headerpad_max_install_names`.
    pub headerpad_max_install_names: bool,
    /// `-pie` (`Some(true)`) / `-no_pie` (`Some(false)`).
    pub pie: Option<bool>,
    /// `-dead_strip_dylibs`: drop dylibs nothing binds to.
    pub dead_strip_dylibs: bool,
    /// `-fixup_chains` (`Some(true)`) / `-no_fixup_chains` (`Some(false)`).
    /// `None` chooses from the deployment target.
    pub fixup_chains: Option<bool>,
    /// `-no_uuid`.
    pub uuid: UuidMode,
    /// `-image_base` / `-seg1addr`.
    pub image_base: Option<u64>,
    /// `-pagezero_size`.
    pub pagezero_size: Option<u64>,
    /// `-stack_size`.
    pub stack_size: Option<u64>,
    /// `-sectcreate <segment> <section> <file>`.
    pub sectcreate: Vec<(String, String, PathBuf)>,
    /// `-alias <symbol> <alias>`.
    pub aliases: Vec<(String, String)>,
    /// `-mark_dead_strippable_dylib`.
    pub mark_dead_strippable_dylib: bool,
    /// `-oso_prefix`: removed from the object paths of the debug map.
    pub oso_prefix: Option<PathBuf>,
    /// `-function_starts` (default) / `-no_function_starts`.
    pub function_starts: bool,
    /// `-data_in_code_info` (default) / `-no_data_in_code_info`.
    pub data_in_code: bool,
    /// `-lto_library`: recorded; Mach-O LTO is not implemented.
    pub lto_library: Option<PathBuf>,
    /// `-bundle_loader`: the executable a bundle is loaded into.
    pub bundle_loader: Option<PathBuf>,
    /// `-v` given together with a link: print the version first.
    pub print_version: bool,
    /// `-keep_private_externs`: with `-r`, private externs stay private
    /// externs instead of becoming local symbols.
    pub keep_private_externs: bool,
    /// `-flat_namespace` (`true`) / `-twolevel_namespace` (`false`, the
    /// default). `-force_flat_namespace` sets it too.
    pub flat_namespace: bool,
    /// `-force_flat_namespace`: an executable that makes dyld bind every
    /// image it loads with flat lookup (`MH_FORCE_FLAT`).
    pub force_flat_namespace: bool,
}

impl Default for DarwinArgs {
    fn default() -> Self {
        Self {
            archs: Vec::new(),
            output_type: MachOutputType::Execute,
            platform: None,
            syslibroots: Vec::new(),
            framework_paths: Vec::new(),
            no_default_search_paths: false,
            search_dylibs_first: false,
            inputs: Vec::new(),
            all_load: false,
            objc: false,
            current_version: None,
            compatibility_version: None,
            undefined: UndefinedTreatment::Error,
            dynamic_lookup_symbols: Vec::new(),
            exported_symbols_lists: Vec::new(),
            unexported_symbols_lists: Vec::new(),
            exported_symbols: Vec::new(),
            unexported_symbols: Vec::new(),
            no_exported_symbols: false,
            order_file: None,
            adhoc_codesign: None,
            headerpad: None,
            headerpad_max_install_names: false,
            pie: None,
            dead_strip_dylibs: false,
            fixup_chains: None,
            uuid: UuidMode::Content,
            image_base: None,
            pagezero_size: None,
            stack_size: None,
            sectcreate: Vec::new(),
            aliases: Vec::new(),
            mark_dead_strippable_dylib: false,
            oso_prefix: None,
            function_starts: true,
            data_in_code: true,
            lto_library: None,
            bundle_loader: None,
            print_version: false,
            keep_private_externs: false,
            flat_namespace: false,
            force_flat_namespace: false,
        }
    }
}

/// How an ld64 option takes its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DarwinArg {
    /// No value.
    Flag,
    /// This many values, as separate arguments.
    Values(u8),
    /// A value joined to the name (`-lSystem`), or as the next argument when
    /// nothing is joined.
    Joined,
}

/// One ld64 option.
#[derive(Clone, Copy, Debug)]
pub struct DarwinOption {
    /// The name without the leading dash.
    pub name: &'static str,
    /// How it takes values.
    pub arg: DarwinArg,
    /// Whether qld implements, ignores or rejects it.
    pub status: Status,
    /// One-line description for `-help`; empty for aliases.
    pub help: &'static str,
    action: Act,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    None,
    Help,
    Version,
    Arch,
    Output,
    KeepPrivateExterns,
    Namespace(bool),
    OutputType(MachOutputType),
    Entry,
    InstallName,
    CurrentVersion,
    CompatibilityVersion,
    Rpath,
    LibraryPath,
    FrameworkPath,
    Library(LoadMode),
    Framework(LoadMode),
    LibraryFile(LoadMode),
    ForceLoad,
    AllLoad,
    ObjC,
    Syslibroot,
    NoDefaultPaths,
    SearchDylibsFirst,
    PlatformVersion,
    VersionMin(u32),
    SdkVersion,
    DeadStrip,
    DeadStripDylibs,
    Undefined,
    DynamicLookupSymbol,
    RequireSymbol,
    ExportedList,
    UnexportedList,
    Exported,
    Unexported,
    NoExported,
    OrderFile,
    LtoLibrary,
    Demangle,
    AdhocCodesign(bool),
    Headerpad,
    HeaderpadMax,
    Pie(bool),
    FixupChains(bool),
    NoUuid,
    StripDebugMap,
    DiscardLocals(DiscardMode),
    Filelist,
    ImageBase,
    PagezeroSize,
    StackSize,
    Sectcreate,
    Alias,
    AliasList,
    ForceFlat,
    Init,
    DeadStrippableDylib,
    OsoPrefix,
    FunctionStarts(bool),
    DataInCode(bool),
    NoWarnings,
    FatalWarnings,
    BundleLoader,
    Trace,
}

const fn opt(name: &'static str, arg: DarwinArg, action: Act, help: &'static str) -> DarwinOption {
    DarwinOption {
        name,
        arg,
        status: Status::Implemented,
        help,
        action,
    }
}

const fn ignored(name: &'static str, arg: DarwinArg) -> DarwinOption {
    DarwinOption {
        name,
        arg,
        status: Status::Ignored,
        help: "",
        action: Act::None,
    }
}

const fn unsupported(name: &'static str, arg: DarwinArg, why: &'static str) -> DarwinOption {
    DarwinOption {
        name,
        arg,
        status: Status::Unsupported(why),
        help: "",
        action: Act::None,
    }
}

use DarwinArg::{Flag, Joined, Values};

const V1: DarwinArg = Values(1);
const V2: DarwinArg = Values(2);
const V3: DarwinArg = Values(3);

/// Every ld64 option qld recognizes.
pub const DARWIN_OPTIONS: &[DarwinOption] = &[
    // Informational.
    opt("help", Flag, Act::Help, "Print this help"),
    opt("-help", Flag, Act::Help, ""),
    opt("v", Flag, Act::Version, "Print the version"),
    opt("-version", Flag, Act::Version, ""),
    opt("version_details", Flag, Act::Version, ""),
    // Output.
    opt("o", V1, Act::Output, "Output file (default a.out)"),
    opt(
        "arch",
        V1,
        Act::Arch,
        "Architecture; repeat for a universal binary",
    ),
    opt(
        "arch_multiple",
        Flag,
        Act::None,
        "Name the architecture in messages",
    ),
    opt(
        "execute",
        Flag,
        Act::OutputType(MachOutputType::Execute),
        "Produce an executable (default)",
    ),
    opt(
        "dylib",
        Flag,
        Act::OutputType(MachOutputType::Dylib),
        "Produce a dynamic library",
    ),
    opt(
        "bundle",
        Flag,
        Act::OutputType(MachOutputType::Bundle),
        "Produce a loadable bundle",
    ),
    opt("dynamic", Flag, Act::None, "Link against dylibs (default)"),
    unsupported("static", Flag, "static Mach-O executables"),
    opt(
        "r",
        Flag,
        Act::OutputType(MachOutputType::Object),
        "Produce a relocatable object",
    ),
    unsupported("preload", Flag, "MH_PRELOAD output"),
    unsupported("kext", Flag, "kernel extensions"),
    opt("e", V1, Act::Entry, "Entry point symbol (default _main)"),
    opt(
        "install_name",
        V1,
        Act::InstallName,
        "Install name of a dylib",
    ),
    opt("dylib_install_name", V1, Act::InstallName, ""),
    opt(
        "current_version",
        V1,
        Act::CurrentVersion,
        "Current version of a dylib",
    ),
    opt("dylib_current_version", V1, Act::CurrentVersion, ""),
    opt(
        "compatibility_version",
        V1,
        Act::CompatibilityVersion,
        "Compatibility version of a dylib",
    ),
    opt(
        "dylib_compatibility_version",
        V1,
        Act::CompatibilityVersion,
        "",
    ),
    opt("rpath", V1, Act::Rpath, "Add an LC_RPATH run path"),
    opt(
        "pie",
        Flag,
        Act::Pie(true),
        "Position-independent executable (default)",
    ),
    opt("no_pie", Flag, Act::Pie(false), "Non-PIE executable"),
    opt(
        "fixup_chains",
        Flag,
        Act::FixupChains(true),
        "Use LC_DYLD_CHAINED_FIXUPS",
    ),
    opt(
        "no_fixup_chains",
        Flag,
        Act::FixupChains(false),
        "Use LC_DYLD_INFO_ONLY",
    ),
    opt(
        "adhoc_codesign",
        Flag,
        Act::AdhocCodesign(true),
        "Sign the output ad hoc",
    ),
    opt(
        "no_adhoc_codesign",
        Flag,
        Act::AdhocCodesign(false),
        "Do not sign the output",
    ),
    opt(
        "headerpad",
        V1,
        Act::Headerpad,
        "Space to leave after the load commands",
    ),
    opt(
        "headerpad_max_install_names",
        Flag,
        Act::HeaderpadMax,
        "Leave room to rewrite install names",
    ),
    opt("no_uuid", Flag, Act::NoUuid, "Omit LC_UUID"),
    unsupported(
        "random_uuid",
        Flag,
        "random LC_UUID (qld output is deterministic)",
    ),
    opt(
        "image_base",
        V1,
        Act::ImageBase,
        "Base address of the image",
    ),
    opt("seg1addr", V1, Act::ImageBase, ""),
    opt("pagezero_size", V1, Act::PagezeroSize, "Size of __PAGEZERO"),
    opt("stack_size", V1, Act::StackSize, "Main thread stack size"),
    opt(
        "sectcreate",
        V3,
        Act::Sectcreate,
        "Create a section from a file",
    ),
    opt("segcreate", V3, Act::Sectcreate, ""),
    opt(
        "mark_dead_strippable_dylib",
        Flag,
        Act::DeadStrippableDylib,
        "Mark a dylib as removable when unused",
    ),
    opt("function_starts", Flag, Act::FunctionStarts(true), ""),
    opt(
        "no_function_starts",
        Flag,
        Act::FunctionStarts(false),
        "Omit LC_FUNCTION_STARTS",
    ),
    opt("data_in_code_info", Flag, Act::DataInCode(true), ""),
    opt(
        "no_data_in_code_info",
        Flag,
        Act::DataInCode(false),
        "Omit LC_DATA_IN_CODE",
    ),
    opt(
        "bundle_loader",
        V1,
        Act::BundleLoader,
        "Executable a bundle links against",
    ),
    // Platform.
    opt(
        "platform_version",
        V3,
        Act::PlatformVersion,
        "Platform, minimum OS and SDK versions",
    ),
    opt(
        "macos_version_min",
        V1,
        Act::VersionMin(consts::PLATFORM_MACOS),
        "",
    ),
    opt(
        "macosx_version_min",
        V1,
        Act::VersionMin(consts::PLATFORM_MACOS),
        "",
    ),
    opt(
        "ios_version_min",
        V1,
        Act::VersionMin(consts::PLATFORM_IOS),
        "",
    ),
    opt(
        "iphoneos_version_min",
        V1,
        Act::VersionMin(consts::PLATFORM_IOS),
        "",
    ),
    opt(
        "ios_simulator_version_min",
        V1,
        Act::VersionMin(consts::PLATFORM_IOSSIMULATOR),
        "",
    ),
    opt(
        "tvos_version_min",
        V1,
        Act::VersionMin(consts::PLATFORM_TVOS),
        "",
    ),
    opt(
        "watchos_version_min",
        V1,
        Act::VersionMin(consts::PLATFORM_WATCHOS),
        "",
    ),
    opt("sdk_version", V1, Act::SdkVersion, ""),
    // Inputs.
    opt(
        "l",
        Joined,
        Act::Library(LoadMode::Normal),
        "Link lib<name>.tbd, .dylib or .a",
    ),
    opt(
        "weak-l",
        Joined,
        Act::Library(LoadMode::Weak),
        "Link a library weakly",
    ),
    opt(
        "reexport-l",
        Joined,
        Act::Library(LoadMode::Reexport),
        "Link and re-export a library",
    ),
    opt(
        "needed-l",
        Joined,
        Act::Library(LoadMode::Needed),
        "Link a library even if unused",
    ),
    opt(
        "hidden-l",
        Joined,
        Act::Library(LoadMode::Hidden),
        "Link an archive, hiding its symbols",
    ),
    opt("lazy-l", Joined, Act::Library(LoadMode::Normal), ""),
    opt(
        "framework",
        V1,
        Act::Framework(LoadMode::Normal),
        "Link a framework",
    ),
    opt(
        "weak_framework",
        V1,
        Act::Framework(LoadMode::Weak),
        "Link a framework weakly",
    ),
    opt(
        "reexport_framework",
        V1,
        Act::Framework(LoadMode::Reexport),
        "Link and re-export a framework",
    ),
    opt(
        "needed_framework",
        V1,
        Act::Framework(LoadMode::Needed),
        "Link a framework even if unused",
    ),
    opt("lazy_framework", V1, Act::Framework(LoadMode::Normal), ""),
    opt(
        "weak_library",
        V1,
        Act::LibraryFile(LoadMode::Weak),
        "Link a library file weakly",
    ),
    opt(
        "reexport_library",
        V1,
        Act::LibraryFile(LoadMode::Reexport),
        "Link and re-export a library file",
    ),
    opt(
        "needed_library",
        V1,
        Act::LibraryFile(LoadMode::Needed),
        "Link a library file even if unused",
    ),
    opt("lazy_library", V1, Act::LibraryFile(LoadMode::Normal), ""),
    opt(
        "force_load",
        V1,
        Act::ForceLoad,
        "Load every member of an archive",
    ),
    opt(
        "all_load",
        Flag,
        Act::AllLoad,
        "Load every member of every archive",
    ),
    opt("noall_load", Flag, Act::None, ""),
    opt(
        "ObjC",
        Flag,
        Act::ObjC,
        "Load archive members with Objective-C code",
    ),
    opt(
        "filelist",
        V1,
        Act::Filelist,
        "Read input paths from a file (file[,dir])",
    ),
    opt(
        "L",
        Joined,
        Act::LibraryPath,
        "Add a library search directory",
    ),
    opt(
        "F",
        Joined,
        Act::FrameworkPath,
        "Add a framework search directory",
    ),
    opt(
        "syslibroot",
        V1,
        Act::Syslibroot,
        "Prefix for search directories",
    ),
    opt(
        "Z",
        Flag,
        Act::NoDefaultPaths,
        "Do not search default directories",
    ),
    opt(
        "search_paths_first",
        Flag,
        Act::None,
        "Search each directory for dylibs and archives (default)",
    ),
    opt(
        "search_dylibs_first",
        Flag,
        Act::SearchDylibsFirst,
        "Search all directories for dylibs first",
    ),
    // Symbols.
    opt(
        "dead_strip",
        Flag,
        Act::DeadStrip,
        "Remove unreachable code and data",
    ),
    opt(
        "dead_strip_dylibs",
        Flag,
        Act::DeadStripDylibs,
        "Drop dylibs nothing uses",
    ),
    opt("no_dead_strip_inits_and_terms", Flag, Act::None, ""),
    opt(
        "undefined",
        V1,
        Act::Undefined,
        "error, warning, suppress or dynamic_lookup",
    ),
    opt(
        "U",
        V1,
        Act::DynamicLookupSymbol,
        "Allow a symbol to be undefined",
    ),
    opt(
        "u",
        V1,
        Act::RequireSymbol,
        "Require a symbol to be defined",
    ),
    opt(
        "exported_symbols_list",
        V1,
        Act::ExportedList,
        "Export only the symbols listed in a file",
    ),
    opt(
        "unexported_symbols_list",
        V1,
        Act::UnexportedList,
        "Do not export the symbols listed in a file",
    ),
    opt(
        "exported_symbol",
        V1,
        Act::Exported,
        "Export only matching symbols",
    ),
    opt(
        "unexported_symbol",
        V1,
        Act::Unexported,
        "Do not export matching symbols",
    ),
    opt(
        "no_exported_symbols",
        Flag,
        Act::NoExported,
        "Export nothing",
    ),
    opt("alias", V2, Act::Alias, "Define an alias of a symbol"),
    opt("init", V1, Act::Init, "Initializer function"),
    opt(
        "order_file",
        V1,
        Act::OrderFile,
        "Order symbols as listed in a file",
    ),
    opt(
        "twolevel_namespace",
        Flag,
        Act::Namespace(false),
        "Two-level namespace (default)",
    ),
    opt(
        "flat_namespace",
        Flag,
        Act::Namespace(true),
        "Flat namespace: imports are looked up by name in every image",
    ),
    opt(
        "force_flat_namespace",
        Flag,
        Act::ForceFlat,
        "Flat namespace for this executable and every image it loads",
    ),
    ignored("multiply_defined", V1),
    ignored("multiply_defined_unused", V1),
    ignored("weak_reference_mismatches", V1),
    ignored("commons", V1),
    ignored("warn_commons", Flag),
    opt(
        "keep_private_externs",
        Flag,
        Act::KeepPrivateExterns,
        "With -r, keep private externs instead of making them local",
    ),
    opt(
        "alias_list",
        V1,
        Act::AliasList,
        "Define aliases listed in a file (symbol alias, one per line)",
    ),
    unsupported("interposable", Flag, "interposable symbols"),
    unsupported("interposable_list", V1, "interposable symbols"),
    unsupported("reexported_symbols_list", V1, "-reexported_symbols_list"),
    // Symbol table and debug information.
    opt("S", Flag, Act::StripDebugMap, "Do not write the debug map"),
    opt(
        "x",
        Flag,
        Act::DiscardLocals(DiscardMode::All),
        "Omit local symbols",
    ),
    opt(
        "X",
        Flag,
        Act::DiscardLocals(DiscardMode::Locals),
        "Omit temporary local symbols",
    ),
    ignored("s", Flag),
    opt(
        "oso_prefix",
        V1,
        Act::OsoPrefix,
        "Remove a prefix from debug map object paths",
    ),
    ignored("add_ast_path", V1),
    ignored("demangle", Flag),
    opt("no_demangle", Flag, Act::Demangle, ""),
    ignored("no_eh_labels", Flag),
    ignored("keep_dwarf_unwind", Flag),
    ignored("no_keep_dwarf_unwind", Flag),
    ignored("no_compact_unwind", Flag),
    // Diagnostics.
    opt("w", Flag, Act::NoWarnings, "Suppress warnings"),
    opt(
        "fatal_warnings",
        Flag,
        Act::FatalWarnings,
        "Treat warnings as errors",
    ),
    opt("t", Flag, Act::Trace, "Print each input file"),
    unsupported("map", V1, "link maps for Mach-O"),
    ignored("why_load", Flag),
    ignored("print_statistics", Flag),
    ignored("arch_errors_fatal", Flag),
    ignored("warn_duplicate_libraries", Flag),
    ignored("no_warn_duplicate_libraries", Flag),
    ignored("no_warn_inits", Flag),
    ignored("warn_weak_exports", Flag),
    ignored("debug_variant", Flag),
    ignored("application_extension", Flag),
    ignored("no_application_extension", Flag),
    ignored("dependency_info", V1),
    ignored("final_output", V1),
    ignored("reproducible", Flag),
    // Behavior qld has anyway.
    ignored("no_deduplicate", Flag),
    ignored("deduplicate", Flag),
    ignored("bind_at_load", Flag),
    ignored("no_implicit_dylibs", Flag),
    ignored("no_objc_category_merging", Flag),
    ignored("objc_category_merging", Flag),
    ignored("objc_abi_version", V1),
    ignored("ld_classic", Flag),
    // Obsolete options ld64 accepts and ignores.
    ignored("single_module", Flag),
    ignored("multi_module", Flag),
    ignored("prebind", Flag),
    ignored("noprebind", Flag),
    ignored("nofixprebinding", Flag),
    ignored("twolevel_namespace_hints", Flag),
    ignored("nomultidefs", Flag),
    ignored("whatsloaded", Flag),
    ignored("force_cpusubtype_ALL", Flag),
    ignored("seglinkedit", Flag),
    ignored("noseglinkedit", Flag),
    ignored("ld_new", Flag),
    ignored("merge_zero_fill_sections", Flag),
    ignored("ignore_optimization_hints", Flag),
    ignored("source_version", V1),
    ignored("no_source_version", Flag),
    ignored("no_weak_imports", Flag),
    ignored("dylib_file", V1),
    ignored("thread_count", V1),
    // LTO: Mach-O LTO is not implemented, so these only matter for bitcode
    // inputs, which the driver rejects by name.
    opt(
        "lto_library",
        V1,
        Act::LtoLibrary,
        "libLTO to use for bitcode inputs",
    ),
    ignored("object_path_lto", V1),
    ignored("cache_path_lto", V1),
    ignored("prune_interval_lto", V1),
    ignored("prune_after_lto", V1),
    ignored("max_relative_cache_size_lto", V1),
    ignored("mllvm", V1),
    ignored("mcpu", V1),
    ignored("export_dynamic", Flag),
    ignored("flto-codegen-only", Flag),
    // Rejected.
    unsupported("bitcode_bundle", Flag, "bitcode bundles"),
    unsupported("sectalign", V3, "-sectalign"),
    unsupported("sectorder", V3, "-sectorder"),
    unsupported("segaddr", V2, "-segaddr"),
    unsupported("segprot", V3, "-segprot"),
    unsupported("rename_section", Values(4), "-rename_section"),
    unsupported("rename_segment", V2, "-rename_segment"),
    unsupported("move_to_rw_segment", V2, "-move_to_rw_segment"),
    unsupported("move_to_ro_segment", V2, "-move_to_ro_segment"),
    unsupported("umbrella", V1, "-umbrella"),
    unsupported("allowable_client", V1, "-allowable_client"),
    unsupported("client_name", V1, "-client_name"),
    unsupported("sub_library", V1, "-sub_library"),
    unsupported("sub_umbrella", V1, "-sub_umbrella"),
    unsupported("dyld_env", V1, "-dyld_env"),
    unsupported("dylinker_install_name", V1, "-dylinker_install_name"),
    unsupported("dtrace", V1, "DTrace static probes"),
    unsupported("read_only_relocs", V1, "-read_only_relocs"),
    unsupported("exported_symbols_order", V1, "-exported_symbols_order"),
    unsupported("no_branch_islands", Flag, "-no_branch_islands"),
    unsupported("sectobjectsymbols", V2, "-sectobjectsymbols"),
    unsupported("add_empty_section", V2, "-add_empty_section"),
];

/// Finds an option by name (without the dash).
#[must_use]
pub fn find_option(name: &str) -> Option<&'static DarwinOption> {
    DARWIN_OPTIONS.iter().find(|o| o.name == name)
}

/// Returns the `-help` text of the ld64 flavor.
#[must_use]
pub fn darwin_usage() -> String {
    let mut out = String::from("Usage: ld64.qld [options] file...\n\nOptions:\n");
    for option in DARWIN_OPTIONS {
        if option.status != Status::Implemented || option.help.is_empty() {
            continue;
        }
        let name = match option.arg {
            Flag => format!("-{}", option.name),
            Joined => format!("-{}<value>", option.name),
            Values(1) => format!("-{} <value>", option.name),
            Values(n) => format!("-{} <{n} values>", option.name),
        };
        out.push_str(&format!("  {name:<36} {}\n", option.help));
    }
    out
}

/// Parses an ld64 command line (without `argv[0]` and any `-flavor`),
/// expanding `@response` and `-filelist` files through `reader`.
///
/// # Errors
///
/// [`Error::Option`] for unknown, unsupported or malformed options, and
/// [`Error::Io`] for unreadable file lists.
pub fn parse(args: &[&OsStr], reader: &dyn FileReader) -> Result<ParseOutcome> {
    let raw: Vec<Vec<u8>> = args.iter().map(|a| a.as_encoded_bytes().to_vec()).collect();
    let mut expanded = Vec::with_capacity(raw.len());
    expand_responses(raw, reader, 0, &mut expanded)?;
    // `-help` and a lone `-v` print and exit whatever else is there.
    if expanded.iter().any(|a| a == b"-help" || a == b"--help") {
        return Ok(ParseOutcome::Help);
    }
    let mut state = Parser {
        options: LinkOptions::new(),
        reader,
    };
    state.options.flavor = Flavor::Darwin;
    let mut index = 0;
    let mut version = false;
    let mut any_input = false;
    while let Some(arg) = expanded.get(index) {
        index = index.saturating_add(1);
        let text = bytes_to_string(arg)?;
        let Some(name) = text.strip_prefix('-').filter(|n| !n.is_empty()) else {
            state.push(
                DarwinInputKind::File(PathBuf::from(os(arg))),
                LoadMode::Normal,
            );
            any_input = true;
            continue;
        };
        // `-O<level>`: Apple clang passes the compiler's optimization level
        // to a linker named with `-fuse-ld=<path>`, and ld64.lld accepts
        // it. It has no effect on a Mach-O link.
        if is_optimization_level(name) {
            state
                .options
                .ignored
                .push(OsString::from(format!("-{name}")));
            continue;
        }
        let (option, joined) = lookup(name)?;
        let mut values: Vec<String> = Vec::new();
        match option.arg {
            Flag => {}
            Joined => match joined {
                Some(value) if !value.is_empty() => values.push(value.to_owned()),
                _ => {
                    let value = expanded.get(index).ok_or_else(|| missing(option.name))?;
                    index = index.saturating_add(1);
                    values.push(bytes_to_string(value)?);
                }
            },
            Values(count) => {
                for _ in 0..count {
                    let value = expanded.get(index).ok_or_else(|| missing(option.name))?;
                    index = index.saturating_add(1);
                    values.push(bytes_to_string(value)?);
                }
            }
        }
        match option.status {
            Status::Unsupported(why) => {
                return Err(Error::Option(format!(
                    "-{}: not supported by qld ({why})",
                    option.name
                )));
            }
            Status::Ignored => {
                state
                    .options
                    .ignored
                    .push(OsString::from(format!("-{}", option.name)));
                continue;
            }
            Status::Implemented => {}
        }
        if option.action == Act::Version {
            version = true;
            continue;
        }
        if matches!(
            option.action,
            Act::Library(_)
                | Act::Framework(_)
                | Act::LibraryFile(_)
                | Act::ForceLoad
                | Act::Filelist
        ) {
            any_input = true;
        }
        state.apply(option, &values)?;
    }
    if version {
        if !any_input {
            return Ok(ParseOutcome::Version);
        }
        state.options.darwin.print_version = true;
    }
    state.finish()?;
    Ok(ParseOutcome::Link(Box::new(state.options)))
}

/// Expands `@file` arguments. Unlike the GNU flavor, an `@` argument whose
/// file cannot be read stays literal, as in lld: install names and run paths
/// start with `@rpath`, `@loader_path` and `@executable_path`.
fn expand_responses(
    args: Vec<Vec<u8>>,
    reader: &dyn FileReader,
    depth: usize,
    out: &mut Vec<Vec<u8>>,
) -> Result<()> {
    for arg in args {
        let Some(path) = arg.strip_prefix(b"@") else {
            out.push(arg);
            continue;
        };
        let is_dyld_path = [&b"rpath/"[..], b"loader_path/", b"executable_path/"]
            .iter()
            .any(|prefix| path.starts_with(prefix));
        if is_dyld_path || path.is_empty() {
            out.push(arg);
            continue;
        }
        let file = PathBuf::from(os(path));
        match reader.read_file(&file) {
            Ok(contents) => {
                if depth >= 20 {
                    return Err(Error::Option(format!(
                        "{}: response files nested too deeply",
                        file.display()
                    )));
                }
                let tokens = response::tokenize(&contents, Quoting::host_default());
                expand_responses(tokens, reader, depth.saturating_add(1), out)?;
            }
            Err(_) => out.push(arg),
        }
    }
    Ok(())
}

struct Parser<'r> {
    options: LinkOptions,
    reader: &'r dyn FileReader,
}

fn os(bytes: &[u8]) -> OsString {
    // The bytes came from `as_encoded_bytes` or a response file; either way
    // they are what the platform's `OsString` holds.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn bytes_to_string(bytes: &[u8]) -> Result<String> {
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn missing(name: &str) -> Error {
    Error::Option(format!("-{name}: missing argument"))
}

/// Whether `name` (without its dash) is an optimization level: `O`, `O0`
/// to `O3` (any number), `Os`, `Oz` or `Ofast`.
fn is_optimization_level(name: &str) -> bool {
    let Some(level) = name.strip_prefix('O') else {
        return false;
    };
    level.bytes().all(|b| b.is_ascii_digit()) || matches!(level, "s" | "z" | "fast")
}

/// Finds the option for `name` (the argument without its dash), returning
/// the joined value of `-l`-style options.
fn lookup(name: &str) -> Result<(&'static DarwinOption, Option<&str>)> {
    if let Some(option) = find_option(name) {
        return Ok((option, None));
    }
    // Joined options, longest prefix first so `-weak-l` is not `-w`.
    let mut best: Option<&'static DarwinOption> = None;
    for option in DARWIN_OPTIONS {
        if option.arg == Joined
            && name.starts_with(option.name)
            && best.is_none_or(|b| b.name.len() < option.name.len())
        {
            best = Some(option);
        }
    }
    match best {
        Some(option) => Ok((option, name.get(option.name.len()..))),
        None => Err(Error::Option(format!("unknown option: -{name}"))),
    }
}

fn parse_version(option: &str, text: &str) -> Result<PackedVersion> {
    PackedVersion::parse(text)
        .ok_or_else(|| Error::Option(format!("-{option}: malformed version: {text}")))
}

fn parse_number(option: &str, text: &str) -> Result<u64> {
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        // ld64 reads these values as hexadecimal even without `0x`.
        None => u64::from_str_radix(text, 16).ok(),
    };
    parsed.ok_or_else(|| Error::Option(format!("-{option}: malformed number: {text}")))
}

/// Parses an ld64 platform name or number.
fn parse_platform(text: &str) -> Option<u32> {
    let named = match text {
        "macos" | "macosx" | "osx" => consts::PLATFORM_MACOS,
        "ios" => consts::PLATFORM_IOS,
        "tvos" => consts::PLATFORM_TVOS,
        "watchos" => consts::PLATFORM_WATCHOS,
        "bridgeos" => consts::PLATFORM_BRIDGEOS,
        "mac-catalyst" | "maccatalyst" => consts::PLATFORM_MACCATALYST,
        "ios-simulator" | "iossimulator" => consts::PLATFORM_IOSSIMULATOR,
        "tvos-simulator" | "tvossimulator" => consts::PLATFORM_TVOSSIMULATOR,
        "watchos-simulator" | "watchossimulator" => consts::PLATFORM_WATCHOSSIMULATOR,
        "driverkit" => consts::PLATFORM_DRIVERKIT,
        "xros" | "visionos" => consts::PLATFORM_XROS,
        "xros-simulator" | "visionos-simulator" => consts::PLATFORM_XROS_SIMULATOR,
        _ => return text.parse().ok().filter(|&n| n != 0),
    };
    Some(named)
}

impl Parser<'_> {
    fn push(&mut self, kind: DarwinInputKind, mode: LoadMode) {
        self.options.darwin.inputs.push(DarwinInput {
            kind,
            mode,
            force_load: false,
        });
    }

    #[allow(clippy::too_many_lines)]
    fn apply(&mut self, option: &DarwinOption, values: &[String]) -> Result<()> {
        let name = option.name;
        let first = values.first().map_or("", String::as_str);
        let darwin = &mut self.options.darwin;
        match option.action {
            Act::None | Act::Help | Act::Version => {}
            Act::Arch => {
                let arch = Arch::from_name(first)
                    .filter(|a| a.architecture().is_some() && a.is_64bit())
                    .ok_or_else(|| {
                        Error::Option(format!(
                            "-arch: unknown or unsupported architecture: {first}"
                        ))
                    })?;
                if !darwin.archs.contains(&arch) {
                    darwin.archs.push(arch);
                }
            }
            Act::Output => self.options.output = Some(PathBuf::from(first)),
            Act::OutputType(kind) => darwin.output_type = kind,
            Act::Entry => self.options.entry = Some(first.to_owned()),
            Act::InstallName => self.options.soname = Some(first.to_owned()),
            Act::CurrentVersion => darwin.current_version = Some(parse_version(name, first)?),
            Act::CompatibilityVersion => {
                darwin.compatibility_version = Some(parse_version(name, first)?);
            }
            Act::Rpath => self.options.rpaths.push(PathBuf::from(first)),
            Act::LibraryPath => self.options.search_paths.push(PathBuf::from(first)),
            Act::FrameworkPath => darwin.framework_paths.push(PathBuf::from(first)),
            Act::Library(mode) => self.push(DarwinInputKind::Library(first.to_owned()), mode),
            Act::Framework(mode) => {
                let (name, suffix) = match first.split_once(',') {
                    Some((name, suffix)) => (name.to_owned(), Some(suffix.to_owned())),
                    None => (first.to_owned(), None),
                };
                self.push(DarwinInputKind::Framework { name, suffix }, mode);
            }
            Act::LibraryFile(mode) => self.push(DarwinInputKind::File(PathBuf::from(first)), mode),
            Act::ForceLoad => darwin.inputs.push(DarwinInput {
                kind: DarwinInputKind::File(PathBuf::from(first)),
                mode: LoadMode::Normal,
                force_load: true,
            }),
            Act::AllLoad => darwin.all_load = true,
            Act::ObjC => darwin.objc = true,
            Act::Syslibroot => darwin.syslibroots.push(PathBuf::from(first)),
            Act::NoDefaultPaths => darwin.no_default_search_paths = true,
            Act::SearchDylibsFirst => darwin.search_dylibs_first = true,
            Act::PlatformVersion => {
                let platform = parse_platform(first).ok_or_else(|| {
                    Error::Option(format!("-platform_version: unknown platform: {first}"))
                })?;
                let min = parse_version(name, values.get(1).map_or("", String::as_str))?;
                let sdk = parse_version(name, values.get(2).map_or("", String::as_str))?;
                darwin.platform = Some(PlatformVersion { platform, min, sdk });
            }
            Act::VersionMin(platform) => {
                let min = parse_version(name, first)?;
                let sdk = darwin.platform.map_or(min, |p| p.sdk);
                darwin.platform = Some(PlatformVersion { platform, min, sdk });
            }
            Act::SdkVersion => {
                let sdk = parse_version(name, first)?;
                match &mut darwin.platform {
                    Some(platform) => platform.sdk = sdk,
                    None => {
                        darwin.platform = Some(PlatformVersion {
                            platform: consts::PLATFORM_MACOS,
                            min: sdk,
                            sdk,
                        });
                    }
                }
            }
            Act::DeadStrip => self.options.gc_sections = true,
            Act::DeadStripDylibs => darwin.dead_strip_dylibs = true,
            Act::Undefined => {
                darwin.undefined = match first {
                    "error" => UndefinedTreatment::Error,
                    "warning" => UndefinedTreatment::Warning,
                    "suppress" => UndefinedTreatment::Suppress,
                    "dynamic_lookup" => UndefinedTreatment::DynamicLookup,
                    other => {
                        return Err(Error::Option(format!(
                            "-undefined: unknown treatment: {other} (expected error, warning, suppress or dynamic_lookup)"
                        )));
                    }
                };
            }
            Act::DynamicLookupSymbol => darwin.dynamic_lookup_symbols.push(first.to_owned()),
            Act::RequireSymbol => self.options.undefined.push(first.to_owned()),
            Act::ExportedList => darwin.exported_symbols_lists.push(PathBuf::from(first)),
            Act::UnexportedList => darwin.unexported_symbols_lists.push(PathBuf::from(first)),
            Act::Exported => darwin.exported_symbols.push(first.to_owned()),
            Act::Unexported => darwin.unexported_symbols.push(first.to_owned()),
            Act::NoExported => darwin.no_exported_symbols = true,
            Act::OrderFile => darwin.order_file = Some(PathBuf::from(first)),
            Act::LtoLibrary => darwin.lto_library = Some(PathBuf::from(first)),
            Act::Demangle => self.options.demangle = false,
            Act::AdhocCodesign(sign) => darwin.adhoc_codesign = Some(sign),
            Act::Headerpad => darwin.headerpad = Some(parse_number(name, first)?),
            Act::HeaderpadMax => darwin.headerpad_max_install_names = true,
            Act::Pie(pie) => darwin.pie = Some(pie),
            Act::FixupChains(chains) => darwin.fixup_chains = Some(chains),
            Act::NoUuid => darwin.uuid = UuidMode::None,
            Act::StripDebugMap => self.options.strip = StripMode::Debug,
            Act::DiscardLocals(mode) => self.options.discard = mode,
            Act::Filelist => self.filelist(first)?,
            Act::ImageBase => darwin.image_base = Some(parse_number(name, first)?),
            Act::PagezeroSize => darwin.pagezero_size = Some(parse_number(name, first)?),
            Act::StackSize => darwin.stack_size = Some(parse_number(name, first)?),
            Act::Sectcreate => {
                let segment = first.to_owned();
                let section = values.get(1).cloned().unwrap_or_default();
                if segment.len() > 16 || section.len() > 16 {
                    return Err(Error::Option(format!(
                        "-{name}: segment and section names are limited to 16 bytes"
                    )));
                }
                let file = PathBuf::from(values.get(2).map_or("", String::as_str));
                darwin.sectcreate.push((segment, section, file));
            }
            Act::Alias => darwin
                .aliases
                .push((first.to_owned(), values.get(1).cloned().unwrap_or_default())),
            Act::Init => self.options.init = Some(first.to_owned()),
            Act::KeepPrivateExterns => darwin.keep_private_externs = true,
            Act::Namespace(flat) => {
                darwin.flat_namespace = flat;
                darwin.force_flat_namespace = false;
            }
            Act::ForceFlat => {
                darwin.flat_namespace = true;
                darwin.force_flat_namespace = true;
            }
            Act::AliasList => self.alias_list(first)?,
            Act::DeadStrippableDylib => darwin.mark_dead_strippable_dylib = true,
            Act::OsoPrefix => darwin.oso_prefix = Some(PathBuf::from(first)),
            Act::FunctionStarts(on) => darwin.function_starts = on,
            Act::DataInCode(on) => darwin.data_in_code = on,
            Act::NoWarnings => self.options.no_warnings = true,
            Act::FatalWarnings => self.options.fatal_warnings = true,
            Act::BundleLoader => darwin.bundle_loader = Some(PathBuf::from(first)),
            Act::Trace => self.options.trace = true,
        }
        Ok(())
    }

    /// `-alias_list file`: one `symbol alias` pair per line, separated by
    /// white space; `#` starts a comment.
    fn alias_list(&mut self, file: &str) -> Result<()> {
        let contents = self
            .reader
            .read_file(std::path::Path::new(file))
            .map_err(|error| Error::io(file, error))?;
        for (number, line) in contents.split(|&b| b == b'\n').enumerate() {
            let line = match line.iter().position(|&b| b == b'#') {
                Some(hash) => line.get(..hash).unwrap_or(&[]),
                None => line,
            };
            let mut words = line
                .split(|b| b.is_ascii_whitespace())
                .filter(|w| !w.is_empty());
            let Some(symbol) = words.next() else {
                continue;
            };
            let (Some(alias), None) = (words.next(), words.next()) else {
                return Err(Error::Option(format!(
                    "-alias_list {file}:{}: expected `symbol alias`",
                    number.saturating_add(1)
                )));
            };
            self.options.darwin.aliases.push((
                String::from_utf8_lossy(symbol).into_owned(),
                String::from_utf8_lossy(alias).into_owned(),
            ));
        }
        Ok(())
    }

    /// `-filelist file[,dir]`: one path per line, relative to `dir` when
    /// given.
    fn filelist(&mut self, value: &str) -> Result<()> {
        let (file, dir) = match value.split_once(',') {
            Some((file, dir)) => (file, Some(PathBuf::from(dir))),
            None => (value, None),
        };
        let contents = self
            .reader
            .read_file(std::path::Path::new(file))
            .map_err(|error| Error::io(file, error))?;
        for line in contents.split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            let path = PathBuf::from(os(line));
            let path = match &dir {
                Some(dir) => dir.join(path),
                None => path,
            };
            self.push(DarwinInputKind::File(path), LoadMode::Normal);
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        let options = &mut self.options;
        if options.darwin.output_type != MachOutputType::Dylib {
            if options.soname.is_some() {
                return Err(Error::Option(
                    "-install_name can only be used with -dylib".into(),
                ));
            }
            if options.darwin.current_version.is_some()
                || options.darwin.compatibility_version.is_some()
            {
                return Err(Error::Option(
                    "-current_version and -compatibility_version can only be used with -dylib"
                        .into(),
                ));
            }
        }
        if options.darwin.force_flat_namespace
            && options.darwin.output_type != MachOutputType::Execute
        {
            return Err(Error::Option(
                "-force_flat_namespace can only be used with main executables".into(),
            ));
        }
        if options.darwin.bundle_loader.is_some()
            && options.darwin.output_type != MachOutputType::Bundle
        {
            return Err(Error::Option(
                "-bundle_loader can only be used with -bundle".into(),
            ));
        }
        if options.init.is_some() && options.darwin.output_type != MachOutputType::Dylib {
            return Err(Error::Option("-init can only be used with -dylib".into()));
        }
        let arch = options
            .darwin
            .archs
            .first()
            .and_then(|a| a.architecture())
            .unwrap_or(crate::target::Architecture::Aarch64);
        options.target = Some(Target {
            format: BinaryFormat::MachO,
            arch,
            endian: Endianness::Little,
            pointer_width: PointerWidth::Bits64,
            os: OperatingSystem::Darwin,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::response::NoFiles;
    use crate::target::Architecture;

    fn run(args: &[&str]) -> Result<ParseOutcome> {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        parse(&args, &NoFiles)
    }

    fn parse_ok(args: &[&str]) -> Box<LinkOptions> {
        match run(args) {
            Ok(ParseOutcome::Link(options)) => options,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    fn parse_err(args: &[&str]) -> String {
        match run(args) {
            Err(error) => error.to_string(),
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn table_names_are_unique() {
        for (index, option) in DARWIN_OPTIONS.iter().enumerate() {
            assert!(
                DARWIN_OPTIONS[..index]
                    .iter()
                    .all(|o| o.name != option.name),
                "duplicate -{}",
                option.name
            );
        }
    }

    #[test]
    fn every_table_entry_parses_or_is_rejected_by_name() {
        for option in DARWIN_OPTIONS {
            if matches!(
                option.action,
                Act::Help | Act::Version | Act::Filelist | Act::AliasList
            ) {
                continue;
            }
            let value = match option.action {
                Act::Arch => "arm64",
                Act::PlatformVersion => "macos",
                Act::Undefined => "error",
                Act::VersionMin(_)
                | Act::SdkVersion
                | Act::CurrentVersion
                | Act::CompatibilityVersion => "1.2",
                Act::Headerpad | Act::ImageBase | Act::PagezeroSize | Act::StackSize => "0x1000",
                _ => "x",
            };
            let mut args = vec![format!("-{}", option.name)];
            match option.arg {
                Flag => {}
                Joined => args[0].push_str(value),
                Values(n) => {
                    args.push(value.to_owned());
                    for _ in 1..n {
                        args.push("1.0".to_owned());
                    }
                }
            }
            if matches!(
                option.action,
                Act::InstallName | Act::CurrentVersion | Act::CompatibilityVersion | Act::Init
            ) {
                args.push("-dylib".to_owned());
            }
            if option.action == Act::BundleLoader {
                args.push("-bundle".to_owned());
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            match option.status {
                Status::Unsupported(_) => {
                    let message = parse_err(&refs);
                    assert!(message.contains(option.name), "{message}");
                }
                Status::Ignored => {
                    let options = parse_ok(&refs);
                    assert_eq!(
                        options.ignored,
                        [OsString::from(format!("-{}", option.name))]
                    );
                }
                Status::Implemented => {
                    parse_ok(&refs);
                }
            }
        }
    }

    #[test]
    fn apple_clang_link_line() {
        // `clang -v hello.c` with Apple clang 15 on an arm64 Mac.
        let options = parse_ok(&[
            "-demangle",
            "-lto_library",
            "/Library/Developer/CommandLineTools/usr/lib/libLTO.dylib",
            "-no_deduplicate",
            "-dynamic",
            "-arch",
            "arm64",
            "-platform_version",
            "macos",
            "14.0.0",
            "14.2",
            "-syslibroot",
            "/Library/Developer/CommandLineTools/SDKs/MacOSX14.sdk",
            "-mllvm",
            "-enable-linkonceodr-outlining",
            "-o",
            "hello",
            "-L/usr/local/lib",
            "/var/folders/xx/T/hello-5c1d9e.o",
            "-lSystem",
            "/Library/Developer/CommandLineTools/usr/lib/clang/15.0.0/lib/darwin/libclang_rt.osx.a",
        ]);
        let darwin = &options.darwin;
        assert_eq!(options.flavor, Flavor::Darwin);
        assert_eq!(darwin.archs, [Arch::ARM64]);
        assert_eq!(
            options.target.map(|t| (t.format, t.arch)),
            Some((BinaryFormat::MachO, Architecture::Aarch64))
        );
        assert_eq!(
            darwin.platform,
            Some(PlatformVersion {
                platform: consts::PLATFORM_MACOS,
                min: PackedVersion::new(14, 0, 0),
                sdk: PackedVersion::new(14, 2, 0),
            })
        );
        assert_eq!(options.output, Some(PathBuf::from("hello")));
        assert_eq!(options.search_paths, [PathBuf::from("/usr/local/lib")]);
        assert_eq!(darwin.inputs.len(), 3);
        assert_eq!(
            darwin.inputs[1].kind,
            DarwinInputKind::Library("System".into())
        );
        assert!(darwin.lto_library.is_some());
    }

    #[test]
    fn optimization_levels_are_ignored() {
        // Apple clang passes `-O<n>` to a linker given with `-fuse-ld=`.
        for level in ["-O", "-O0", "-O2", "-O3", "-Os", "-Oz", "-Ofast"] {
            let options = parse_ok(&["-arch", "arm64", level, "a.o"]);
            assert_eq!(options.ignored, [OsString::from(level)], "{level}");
            assert_eq!(options.darwin.inputs.len(), 1, "{level}");
        }
        assert!(parse_err(&["-Ox", "a.o"]).contains("-Ox"));
        assert!(parse_ok(&["-ObjC", "a.o"]).darwin.objc);
    }

    #[test]
    fn libraries_frameworks_and_modes() {
        let options = parse_ok(&[
            "-dylib",
            "-install_name",
            "@rpath/libfoo.dylib",
            "-current_version",
            "1.2.3",
            "-weak-lfoo",
            "-reexport-l",
            "bar",
            "-framework",
            "Foundation",
            "-weak_framework",
            "AppKit,_debug",
            "-force_load",
            "libx.a",
            "-undefined",
            "dynamic_lookup",
            "-arch",
            "x86_64",
            "-arch",
            "arm64",
            "-rpath",
            "@loader_path",
            "-headerpad",
            "100",
            "-dead_strip",
        ]);
        let darwin = &options.darwin;
        assert_eq!(darwin.output_type, MachOutputType::Dylib);
        assert_eq!(options.soname.as_deref(), Some("@rpath/libfoo.dylib"));
        assert_eq!(darwin.current_version, Some(PackedVersion::new(1, 2, 3)));
        assert_eq!(darwin.inputs[0].mode, LoadMode::Weak);
        assert_eq!(
            darwin.inputs[1].kind,
            DarwinInputKind::Library("bar".into())
        );
        assert_eq!(darwin.inputs[1].mode, LoadMode::Reexport);
        assert_eq!(
            darwin.inputs[3].kind,
            DarwinInputKind::Framework {
                name: "AppKit".into(),
                suffix: Some("_debug".into())
            }
        );
        assert!(darwin.inputs[4].force_load);
        assert_eq!(darwin.undefined, UndefinedTreatment::DynamicLookup);
        assert_eq!(darwin.archs, [Arch::X86_64, Arch::ARM64]);
        assert_eq!(darwin.headerpad, Some(0x100));
        assert!(options.gc_sections);
    }

    #[test]
    fn relocatable_output() {
        let options = parse_ok(&["-r", "-arch", "arm64", "a.o", "b.o", "-o", "ab.o"]);
        assert_eq!(options.darwin.output_type, MachOutputType::Object);
        assert!(!options.darwin.keep_private_externs);
        let options = parse_ok(&["-r", "-keep_private_externs", "a.o"]);
        assert!(options.darwin.keep_private_externs);
    }

    #[test]
    fn errors_name_the_problem() {
        assert!(parse_err(&["-frobnicate"]).contains("-frobnicate"));
        assert!(parse_err(&["-arch", "pdp11"]).contains("pdp11"));
        assert!(parse_err(&["-arch"]).contains("missing"));
        assert!(parse_err(&["-undefined", "maybe"]).contains("maybe"));
        assert!(parse_err(&["-install_name", "x"]).contains("-dylib"));
        assert!(parse_err(&["-platform_version", "plan9", "1", "1"]).contains("plan9"));
    }

    #[test]
    fn version_and_help() {
        assert!(matches!(run(&["-v"]), Ok(ParseOutcome::Version)));
        assert!(matches!(run(&["-help", "a.o"]), Ok(ParseOutcome::Help)));
        match run(&["-v", "a.o"]) {
            Ok(ParseOutcome::Link(options)) => assert!(options.darwin.print_version),
            other => panic!("{other:?}"),
        }
        assert!(darwin_usage().contains("-platform_version"));
    }

    #[test]
    fn alias_list_and_force_flat_namespace() {
        struct List;
        impl FileReader for List {
            fn read_file(&self, _: &std::path::Path) -> std::io::Result<Vec<u8>> {
                Ok(b"# aliases\n_foo _bar\n\n  _baz\t_qux # trailing\n".to_vec())
            }
        }
        let args: Vec<&OsStr> = ["-alias_list", "aliases.txt", "a.o"]
            .iter()
            .map(OsStr::new)
            .collect();
        let Ok(ParseOutcome::Link(options)) = parse(&args, &List) else {
            panic!("-alias_list did not parse");
        };
        assert_eq!(
            options.darwin.aliases,
            [
                ("_foo".to_owned(), "_bar".to_owned()),
                ("_baz".to_owned(), "_qux".to_owned())
            ]
        );

        struct Bad;
        impl FileReader for Bad {
            fn read_file(&self, _: &std::path::Path) -> std::io::Result<Vec<u8>> {
                Ok(b"_foo _bar\n_one\n".to_vec())
            }
        }
        let error = parse(&args, &Bad).unwrap_err().to_string();
        assert!(error.contains("aliases.txt:2"), "{error}");

        let options = parse_ok(&["-force_flat_namespace", "a.o"]);
        assert!(options.darwin.flat_namespace && options.darwin.force_flat_namespace);
        let options = parse_ok(&["-force_flat_namespace", "-twolevel_namespace", "a.o"]);
        assert!(!options.darwin.flat_namespace && !options.darwin.force_flat_namespace);
        assert!(parse_err(&["-force_flat_namespace", "-dylib", "a.o"]).contains("executables"));
    }

    #[test]
    fn filelist_reads_through_the_reader() {
        struct List;
        impl FileReader for List {
            fn read_file(&self, _: &std::path::Path) -> std::io::Result<Vec<u8>> {
                Ok(b"a.o\nb.o\r\n\n".to_vec())
            }
        }
        let args: Vec<&OsStr> = ["-filelist", "list,objs"].iter().map(OsStr::new).collect();
        let Ok(ParseOutcome::Link(options)) = parse(&args, &List) else {
            panic!("filelist did not parse");
        };
        assert_eq!(
            options.darwin.inputs[1].kind,
            DarwinInputKind::File(PathBuf::from("objs/b.o"))
        );
    }
}
