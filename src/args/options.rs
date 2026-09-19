//! The option model: everything a link is configured by.
//!
//! This is the interface between the argv front ends, the library API and the
//! link driver. Adding a field here is how a new option becomes visible to the
//! rest of qld.
//!
//! Every field is plain data. Paths are stored exactly as they were written:
//! a path starting with `=` or `$SYSROOT` keeps that prefix, and
//! [`LinkOptions::resolve_sysroot`] turns it into a real path when the driver
//! needs one. Nothing here touches the file system.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::target::{Endianness, Target};

/// Which command-line dialect an argv is written in.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flavor {
    /// GNU ld, gold, lld and mold (ELF, and PE in MinGW mode).
    #[default]
    Gnu,
    /// Apple ld64.
    Darwin,
}

/// What kind of file the link produces.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputKind {
    /// A dynamically linked, non-position-independent executable.
    #[default]
    Executable,
    /// A position-independent executable (`-pie`).
    Pie,
    /// A shared library (`-shared`).
    Shared,
    /// A statically linked executable (`-static`).
    StaticExecutable,
    /// A statically linked position-independent executable (`-static-pie`).
    StaticPie,
    /// Relocatable output (`-r`).
    Relocatable,
}

/// How much of the symbol table survives into the output.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum StripMode {
    /// Keep everything (default).
    #[default]
    None,
    /// Drop debug sections (`-S`, `--strip-debug`).
    Debug,
    /// Drop debug sections and the symbol table (`-s`, `--strip-all`).
    All,
}

/// Which local symbols are dropped from the output symbol table.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiscardMode {
    /// The linker's default: drop compiler-generated temporary locals
    /// (`.L*`), keep the rest.
    #[default]
    Default,
    /// `-X`, `--discard-locals`: drop temporary locals.
    Locals,
    /// `-x`, `--discard-all`: drop every local symbol.
    All,
    /// `--discard-none`: keep every local symbol.
    None,
}

/// Build ID generation (`--build-id`).
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BuildId {
    /// No build ID note (default).
    #[default]
    None,
    /// A fast non-cryptographic hash of the output.
    Fast,
    /// SHA-1 of the output.
    Sha1,
    /// MD5 of the output.
    Md5,
    /// UUID version 4, which is random rather than derived from the output.
    Uuid,
    /// A caller-supplied value (`--build-id=0x…`).
    Hex(Vec<u8>),
}

/// Which symbol hash tables the dynamic output carries (`--hash-style`).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HashStyle {
    /// `.hash` only.
    Sysv,
    /// `.gnu.hash` only (default).
    #[default]
    Gnu,
    /// Both.
    Both,
}

/// `-Bsymbolic` and its variants: which default-visibility definitions in a
/// shared library bind locally.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SymbolicMode {
    /// `-Bno-symbolic` (default): nothing binds locally.
    #[default]
    None,
    /// `-Bsymbolic`: every definition binds locally.
    All,
    /// `-Bsymbolic-functions`: function definitions bind locally.
    Functions,
    /// `-Bsymbolic-non-weak`: non-weak definitions bind locally.
    NonWeak,
    /// `-Bsymbolic-non-weak-functions`: non-weak function definitions bind
    /// locally.
    NonWeakFunctions,
}

/// `--unresolved-symbols=<method>`.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnresolvedSymbols {
    /// `ignore-all`: report nothing.
    IgnoreAll,
    /// `report-all`: report undefined symbols from objects and shared
    /// libraries.
    ReportAll,
    /// `ignore-in-object-files`: report only those from shared libraries.
    IgnoreInObjectFiles,
    /// `ignore-in-shared-libs`: report only those from object files.
    IgnoreInSharedLibs,
}

/// Colored diagnostics (`--color-diagnostics`).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorChoice {
    /// Color when stderr is a terminal (default).
    #[default]
    Auto,
    /// Always color.
    Always,
    /// Never color.
    Never,
}

/// Page alignment mode (`-n`, `-N`).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MagicMode {
    /// Normal demand-paged output (default).
    #[default]
    Normal,
    /// `-n`, `--nmagic`: do not page-align sections.
    Nmagic,
    /// `-N`, `--omagic`: do not page-align, and make text writable.
    Omagic,
}

/// `-z separate-code` and related segment layout choices.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeparateCode {
    /// `-z noseparate-code`: code may share a segment with other read-only
    /// data.
    None,
    /// `-z separate-code`: code gets its own segment, padded to page
    /// boundaries.
    Code,
    /// `-z separate-loadable-segments`: every loadable segment is padded to
    /// page boundaries.
    Loadable,
}

/// `-z execstack` / `-z noexecstack` / `-z execstack-if-needed`.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecStack {
    /// No option given: decided from the inputs' `.note.GNU-stack` sections
    /// (default).
    #[default]
    FromInputs,
    /// `-z execstack`.
    Executable,
    /// `-z noexecstack`.
    NonExecutable,
}

/// `--call-graph-profile-sort=`: the algorithm that orders sections by call
/// graph profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallGraphSort {
    /// `none`: no ordering by profile.
    None,
    /// `hfsort`: the C3 heuristic (Ottoni and Maher, CGO 2017).
    Hfsort,
    /// `cdsort`: cache-directed sort, lld's default.
    #[default]
    Cdsort,
}

/// Dynamic section flags set by `-z` keywords (`DT_FLAGS` / `DT_FLAGS_1`).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DynamicFlags {
    /// `-z nodelete` (`DF_1_NODELETE`).
    pub nodelete: bool,
    /// `-z nodlopen` (`DF_1_NOOPEN`).
    pub nodlopen: bool,
    /// `-z nodump` (`DF_1_NODUMP`).
    pub nodump: bool,
    /// `-z initfirst` (`DF_1_INITFIRST`).
    pub initfirst: bool,
    /// `-z interpose` (`DF_1_INTERPOSE`).
    pub interpose: bool,
    /// `-z global` (`DF_1_GLOBAL`).
    pub global: bool,
    /// `-z nodefaultlib` (`DF_1_NODEFLIB`).
    pub nodefaultlib: bool,
    /// `-z loadfltr` (`DF_1_LOADFLTR`).
    pub loadfltr: bool,
    /// `-z origin` (`DF_ORIGIN` and `DF_1_ORIGIN`).
    pub origin: bool,
    /// `-z unique` (`DF_1_SINGLETON`).
    pub singleton: bool,
}

/// x86 control-flow enforcement options (`-z ibt`, `-z shstk`, …).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct X86Features {
    /// `-z ibt`: mark the output as IBT-compatible.
    pub ibt: bool,
    /// `-z shstk`: mark the output as shadow-stack-compatible.
    pub shstk: bool,
    /// `-z ibtplt`: generate IBT-enabled PLT entries.
    pub ibtplt: bool,
    /// `-z cet-report=`: how to report inputs missing IBT or SHSTK.
    pub cet_report: ReportLevel,
    /// `-z x86-64-baseline` (1) … `-z x86-64-v4` (4): the ISA level marked
    /// as needed. 0 means none was requested.
    pub isa_level: u8,
}

/// AArch64 branch protection and erratum options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Aarch64Features {
    /// `-z force-bti`: BTI landing pads in the PLT, and the output marked
    /// BTI-compatible, even when an input is not.
    pub force_bti: bool,
    /// `-z pac-plt`: PLT entries authenticate the address they load.
    pub pac_plt: bool,
    /// `--fix-cortex-a53-835769`.
    pub fix_cortex_a53_835769: bool,
}

/// Severity for the `-z *-report=` keywords.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReportLevel {
    /// Report nothing (default).
    #[default]
    None,
    /// Report as a warning.
    Warning,
    /// Report as an error.
    Error,
}

/// `--icf=`: which identical sections are folded together.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IcfMode {
    /// `--icf=none` (default): nothing is folded.
    #[default]
    None,
    /// `--icf=safe`: fold only sections whose addresses cannot be observed.
    Safe,
    /// `--icf=all`: fold every identical section.
    All,
}

/// `--orphan-handling=`: what happens to an input section that no output
/// section description of a linker script claims.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OrphanHandling {
    /// `place` (default): place it next to a section of the same kind.
    #[default]
    Place,
    /// `warn`: place it and warn.
    Warn,
    /// `error`: report an error.
    Error,
    /// `discard`: drop it.
    Discard,
}

/// `--sort-section=`: how the input sections of a wildcard are ordered.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortSection {
    /// Not given (default): only the script's own `SORT_*` keywords sort.
    #[default]
    None,
    /// `name`: sort by input section name.
    Name,
    /// `alignment`: sort by decreasing alignment.
    Alignment,
}

/// `--compress-debug-sections=`: the compression applied to the output's
/// `.debug_*` sections.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DebugCompression {
    /// `none` (default): the debug sections are written uncompressed.
    #[default]
    None,
    /// `zlib`: deflate, in the ELF gABI `SHF_COMPRESSED` form.
    Zlib,
    /// `zlib-gnu`: deflate, in the older `.zdebug_*` form.
    ZlibGnu,
    /// `zlib-gabi`: the same thing as `zlib`, spelled the way GNU ld spells
    /// it.
    ZlibGabi,
    /// `zstd`: Zstandard, in the `SHF_COMPRESSED` form.
    Zstd,
}

impl DebugCompression {
    /// The spelling `--compress-debug-sections` takes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Zlib => "zlib",
            Self::ZlibGnu => "zlib-gnu",
            Self::ZlibGabi => "zlib-gabi",
            Self::Zstd => "zstd",
        }
    }
}

/// An ELF symbol visibility, as `-z start-stop-visibility=` names one.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Visibility {
    /// `default`: `STV_DEFAULT`.
    #[default]
    Default,
    /// `internal`: `STV_INTERNAL`.
    Internal,
    /// `hidden`: `STV_HIDDEN`.
    Hidden,
    /// `protected`: `STV_PROTECTED`.
    Protected,
}

/// `--oformat=`, or a script's `OUTPUT_FORMAT`: the BFD target name of the
/// output.
///
/// The three raw formats have their own variants because the drivers act on
/// them; every other BFD name, such as `elf64-x86-64`, is an
/// [`OutputFormat::Bfd`], which the format driver checks against its own
/// target.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    /// `binary`: the loadable image with no headers.
    Binary,
    /// `ihex`: Intel HEX text.
    Ihex,
    /// `srec`: Motorola S-records.
    Srec,
    /// A BFD target name, such as `elf64-x86-64` or `pei-x86-64`.
    Bfd(String),
}

impl OutputFormat {
    /// The format a BFD target name names. Unknown names become
    /// [`OutputFormat::Bfd`]; the driver reports the ones it cannot write.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name {
            "binary" => Self::Binary,
            "ihex" => Self::Ihex,
            "srec" => Self::Srec,
            other => Self::Bfd(other.to_owned()),
        }
    }

    /// The BFD target name, as it was written.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Binary => "binary",
            Self::Ihex => "ihex",
            Self::Srec => "srec",
            Self::Bfd(name) => name,
        }
    }

    /// Whether this is one of the raw formats, which carry no ELF headers
    /// and no symbol table.
    #[must_use]
    pub fn is_raw(&self) -> bool {
        !matches!(self, Self::Bfd(_))
    }
}

/// How the inputs that follow `-b` / `--format` are interpreted.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum InputFormat {
    /// Identify the format from the file contents (default).
    #[default]
    Auto,
    /// `-b binary`: wrap raw bytes in a data section with
    /// `_binary_<name>_start`/`_end`/`_size` symbols.
    Binary,
}

/// Attributes that positional options attach to the input files that follow
/// them on the command line.
///
/// Build one with [`InputAttrs::default`] and set the fields you need: the
/// struct is `#[non_exhaustive]`, because later milestones keep adding
/// positional options.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct InputAttrs {
    /// `--whole-archive` was in effect. On a Mach-O link this is
    /// `-force_load`, which `-all_load` sets for every input.
    pub whole_archive: bool,
    /// `--as-needed` was in effect.
    pub as_needed: bool,
    /// `-Bstatic` was in effect, so only archives may satisfy this input.
    pub static_only: bool,
    /// The input is inside `--start-group` / `--end-group`.
    pub in_group: bool,
    /// `--copy-dt-needed-entries` was in effect.
    pub copy_dt_needed: bool,
    /// The input is inside `--start-lib` / `--end-lib`: object files are
    /// treated as lazily extracted archive members.
    pub lazy: bool,
    /// The `-b` / `--format` in effect.
    pub format: InputFormat,
    /// How a Mach-O link loads this input (`-weak-l`, `-reexport_library`,
    /// `-needed_framework`, `-hidden-l`). Other formats ignore it.
    pub load: crate::args::darwin::LoadMode,
}

/// What an input entry refers to.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InputKind {
    /// A file named directly on the command line.
    File(PathBuf),
    /// `-lfoo`: search for `libfoo.so` and `libfoo.a` in the search paths.
    /// On a Mach-O link, `libfoo.tbd`, `libfoo.dylib` and `libfoo.a`.
    Library(String),
    /// `-framework Foo[,suffix]`: `Foo.framework/Foo` in the framework
    /// search paths. Mach-O links only.
    Framework {
        /// The framework name.
        name: String,
        /// The optional suffix (`-framework Foo,_debug`).
        suffix: Option<String>,
    },
    /// `-l:libfoo.a`: search for that exact file name.
    LibraryExact(String),
    /// `-T script`, or a script named as an input file.
    Script(PathBuf),
    /// `-R file` / `--just-symbols=file`: take only the symbol values from
    /// this object. GNU ld treats `-R <directory>` as `-rpath`; the driver
    /// makes that distinction, since it needs the file system.
    JustSymbols(PathBuf),
    /// In-memory input, used by library callers.
    Bytes {
        /// Name to show in diagnostics.
        name: String,
        /// File contents.
        data: std::sync::Arc<[u8]>,
    },
}

impl InputKind {
    /// An in-memory input ([`InputKind::Bytes`]) named `name` in
    /// diagnostics.
    ///
    /// `data` is anything that converts into an `Arc<[u8]>`: an
    /// `Arc<[u8]>` is shared as is, a `Vec<u8>` or a `&'static [u8]` is
    /// copied once.
    ///
    /// ```
    /// use qld::args::InputKind;
    ///
    /// static OBJECT: &[u8] = b"\x7fELF...";
    /// let input = InputKind::bytes("embedded.o", OBJECT);
    /// assert!(matches!(input, InputKind::Bytes { .. }));
    /// ```
    #[must_use]
    pub fn bytes(name: impl Into<String>, data: impl Into<std::sync::Arc<[u8]>>) -> Self {
        Self::Bytes {
            name: name.into(),
            data: data.into(),
        }
    }
}

/// One input, with the positional state that applied to it.
///
/// [`LinkOptions::push_input`] is how an input joins a link;
/// [`InputSpec::new`] builds one on its own.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputSpec {
    /// What the input is.
    pub kind: InputKind,
    /// Positional attributes in effect at this point in the command line.
    pub attrs: InputAttrs,
    /// Zero-based position on the command line. Used for precedence decisions
    /// and to order diagnostics deterministically.
    ///
    /// [`LinkOptions::push_input`] assigns it; setting it by hand only makes
    /// sense for a spec that is not in a [`LinkOptions::inputs`] list.
    pub position: usize,
}

impl InputSpec {
    /// One input at position 0, outside any input list.
    #[must_use]
    pub fn new(kind: InputKind, attrs: InputAttrs) -> Self {
        Self {
            kind,
            attrs,
            position: 0,
        }
    }
}

/// The PE/COFF options of GNU ld's MinGW emulations (`i386pep`, `i386pe`,
/// `arm64pe`).
///
/// Every field starts at the `i386pep` default, so a command line that names
/// none of these options still describes the image
/// `x86_64-w64-mingw32-gcc` expects. The PE backend reads them through
/// [`PeOptions::from_link_options`](crate::coff::PeOptions::from_link_options);
/// ELF and Mach-O links ignore them, which is why GNU ld's per-emulation
/// options are accepted whatever the target is.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeArgs {
    /// `--subsystem NAME[,MAJOR[.MINOR]]`, as an `IMAGE_SUBSYSTEM_*` value.
    /// `None` infers it from the entry point.
    pub subsystem: Option<u16>,
    /// `--section-alignment`.
    pub section_alignment: u32,
    /// `--file-alignment`.
    pub file_alignment: u32,
    /// `--stack RESERVE[,COMMIT]`. `-z stack-size` also sets the reserve.
    pub stack: (u64, u64),
    /// `--heap RESERVE[,COMMIT]`.
    pub heap: (u64, u64),
    /// `--major-image-version`.
    pub major_image_version: u16,
    /// `--minor-image-version`.
    pub minor_image_version: u16,
    /// `--major-os-version`.
    pub major_os_version: u16,
    /// `--minor-os-version`.
    pub minor_os_version: u16,
    /// `--major-subsystem-version`, which `--subsystem NAME,MAJOR` also sets.
    pub major_subsystem_version: u16,
    /// `--minor-subsystem-version`, which `--subsystem NAME,MAJOR.MINOR`
    /// also sets.
    pub minor_subsystem_version: u16,
    /// `--dynamicbase` (default) / `--disable-dynamicbase`.
    pub dynamicbase: bool,
    /// `--nxcompat` (default) / `--disable-nxcompat`.
    pub nxcompat: bool,
    /// `--high-entropy-va` (default) / `--disable-high-entropy-va`.
    pub high_entropy_va: bool,
    /// `--tsaware` / `--disable-tsaware` (default).
    pub tsaware: bool,
    /// `--no-seh` / `--disable-no-seh` (default).
    pub no_seh: bool,
    /// `--forceinteg` / `--disable-forceinteg` (default).
    pub forceinteg: bool,
    /// `--no-isolation` / `--disable-no-isolation` (default).
    pub no_isolation: bool,
    /// `--no-bind` / `--disable-no-bind` (default).
    pub no_bind: bool,
    /// `--wdmdriver` / `--disable-wdmdriver` (default).
    pub wdmdriver: bool,
    /// `--large-address-aware` (default) / `--disable-large-address-aware`.
    pub large_address_aware: bool,
    /// `--enable-reloc-section` (default) / `--disable-reloc-section`.
    pub reloc_section: bool,
    /// `--insert-timestamp` / `--no-insert-timestamp` (default, so that the
    /// output is reproducible).
    pub insert_timestamp: bool,
    /// `--out-implib FILE`: write an import library for the exports.
    pub out_implib: Option<PathBuf>,
    /// `--output-def FILE`: write a module-definition file for the exports.
    pub output_def: Option<PathBuf>,
    /// A module-definition file named as a positional input, as GNU ld's PE
    /// emulations accept it.
    pub def_file: Option<PathBuf>,
    /// `--export-all-symbols`.
    pub export_all_symbols: bool,
    /// `--exclude-all-symbols`.
    pub exclude_all_symbols: bool,
    /// `--exclude-symbols SYM,SYM,…`: names `--export-all-symbols` skips.
    pub exclude_symbols: Vec<String>,
    /// `--exclude-modules-for-implib MOD,MOD,…`: objects and archives whose
    /// symbols the import library omits.
    pub exclude_modules_for_implib: Vec<String>,
    /// `--kill-at`: drop the `@N` suffix of stdcall names in exports.
    pub kill_at: bool,
    /// `--add-stdcall-alias`: also export the undecorated name.
    pub add_stdcall_alias: bool,
    /// `--enable-stdcall-fixup` (`Some(true)`) / `--disable-stdcall-fixup`
    /// (`Some(false)`). `None` means the linker decides.
    pub stdcall_fixup: Option<bool>,
    /// `--enable-auto-import` (default) / `--disable-auto-import`.
    pub auto_import: bool,
    /// `--enable-runtime-pseudo-reloc` (default) /
    /// `--disable-runtime-pseudo-reloc`.
    pub runtime_pseudo_reloc: bool,
    /// `--export SPEC`: an export specification in `.drectve` `-export:`
    /// syntax. This spelling is a qld extension; GNU ld takes exports from
    /// `.def` files, `.drectve` sections and `--export-all-symbols`.
    pub exports: Vec<String>,
    /// `--warn-duplicate-exports`.
    pub warn_duplicate_exports: bool,
    /// Which options with a per-emulation default the command line set.
    ///
    /// The fields above start at the `i386pep` defaults; `i386pe` and
    /// `arm64pe` differ in a few of them, and the PE backend applies the
    /// target's own default where the command line said nothing.
    pub explicit: PeExplicit,
}

/// The PE options whose default depends on the emulation, and whether the
/// command line set each of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeExplicit {
    /// `--large-address-aware` or `--disable-large-address-aware`.
    pub large_address_aware: bool,
    /// `--major-os-version` or `--minor-os-version`.
    pub os_version: bool,
    /// `--major-image-version` or `--minor-image-version`.
    pub image_version: bool,
    /// `--major-subsystem-version`, `--minor-subsystem-version`, or a
    /// version in `--subsystem`.
    pub subsystem_version: bool,
}

impl Default for PeArgs {
    fn default() -> Self {
        use crate::coff::options as pe;
        Self {
            subsystem: None,
            section_alignment: pe::DEFAULT_SECTION_ALIGNMENT,
            file_alignment: pe::DEFAULT_FILE_ALIGNMENT,
            stack: (pe::DEFAULT_STACK_RESERVE, pe::DEFAULT_STACK_COMMIT),
            heap: (pe::DEFAULT_HEAP_RESERVE, pe::DEFAULT_HEAP_COMMIT),
            major_image_version: 0,
            minor_image_version: 0,
            major_os_version: 4,
            minor_os_version: 0,
            major_subsystem_version: 5,
            minor_subsystem_version: 2,
            dynamicbase: true,
            nxcompat: true,
            high_entropy_va: true,
            tsaware: false,
            no_seh: false,
            forceinteg: false,
            no_isolation: false,
            no_bind: false,
            wdmdriver: false,
            large_address_aware: true,
            reloc_section: true,
            insert_timestamp: false,
            out_implib: None,
            output_def: None,
            def_file: None,
            export_all_symbols: false,
            exclude_all_symbols: false,
            exclude_symbols: Vec::new(),
            exclude_modules_for_implib: Vec::new(),
            kill_at: false,
            add_stdcall_alias: false,
            stdcall_fixup: None,
            auto_import: true,
            runtime_pseudo_reloc: true,
            exports: Vec::new(),
            warn_duplicate_exports: false,
            explicit: PeExplicit::default(),
        }
    }
}

/// A callback for the moment a successful link's output is complete, set in
/// [`LinkOptions::on_output_complete`].
///
/// When it runs, the output file (and a link map, if one was asked for) is
/// written, closed and in place under its final name; everything the link
/// prints on stdout has been printed; every diagnostic has been emitted; and
/// LTO plugins have been cleaned up. The link then returns `Ok` without
/// emitting anything else: what remains is freeing memory and unmapping the
/// inputs, which takes a while for large links (150 ms for a 1.2 GiB output
/// with debug information).
///
/// It runs at most once per link, and never for a link that fails. Clones
/// share the "already called" state, so the copies a driver makes of the
/// options do not call it twice. It may run on any thread, including a
/// worker of the link's thread pool.
///
/// The `qld` binary uses it for `--fork`: the child process that runs the
/// link tells its parent to exit with success, then cleans up on its own.
#[derive(Clone)]
pub struct OutputCompleteHook {
    callback: std::sync::Arc<dyn Fn() + Send + Sync>,
    called: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl OutputCompleteHook {
    /// Wraps `callback`.
    pub fn new(callback: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            callback: std::sync::Arc::new(callback),
            called: std::sync::Arc::default(),
        }
    }

    /// Runs the callback, unless this hook or a clone of it already did.
    pub fn call(&self) {
        if !self.called.swap(true, std::sync::atomic::Ordering::AcqRel) {
            (self.callback)();
        }
    }
}

impl std::fmt::Debug for OutputCompleteHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputCompleteHook")
            .field(
                "called",
                &self.called.load(std::sync::atomic::Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

/// Receives the output image of a link instead of the output file, set in
/// [`LinkOptions::output_buffer`].
///
/// A link with an output buffer writes nothing at [`LinkOptions::output`]:
/// the image is built in memory and handed to the buffer when the link
/// succeeds. The output path still names the output where a name is needed
/// (the default `DT_SONAME`, the symbols of `-b binary`-style raw outputs).
/// Side outputs such as `-Map` and `--dependency-file` are still files.
///
/// Clones share the image, so keep a clone and read it with
/// [`OutputBuffer::take`] after the link returns `Ok`. The image is stored
/// as soon as it is complete, so a link that fails later (writing a map
/// file, say) can leave one behind; a cancelled link never does.
///
/// The ELF driver supports it, including `-r` and raw formats
/// (`--oformat binary`); the PE and Mach-O drivers do not yet and still
/// write the output file.
///
/// # Example
///
/// ```no_run
/// use qld::args::{LinkOptions, OutputBuffer};
///
/// # fn main() -> qld::Result<()> {
/// let mut options = LinkOptions::new();
/// // ... inputs ...
/// let buffer = OutputBuffer::new();
/// options.output_buffer = Some(buffer.clone());
/// qld::link(&options, &qld::diag::Collect::new())?;
/// let image: Vec<u8> = buffer.take().expect("the link succeeded");
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Default)]
pub struct OutputBuffer {
    image: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
}

impl OutputBuffer {
    /// Creates an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores the output image, replacing any earlier one. Link drivers call
    /// this when the output is complete.
    pub fn store(&self, image: Vec<u8>) {
        match self.image.lock() {
            Ok(mut slot) => *slot = Some(image),
            Err(poisoned) => *poisoned.into_inner() = Some(image),
        }
    }

    /// Takes the output image, leaving the buffer empty. `None` until a link
    /// using this buffer has succeeded.
    #[must_use]
    pub fn take(&self) -> Option<Vec<u8>> {
        match self.image.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }

    /// Whether an image is waiting to be taken.
    #[must_use]
    pub fn is_filled(&self) -> bool {
        match self.image.lock() {
            Ok(slot) => slot.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }
}

impl PartialEq for OutputBuffer {
    /// Two buffers are equal when they are clones of each other.
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.image, &other.image)
    }
}

impl Eq for OutputBuffer {}

impl std::fmt::Debug for OutputBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputBuffer")
            .field("filled", &self.is_filled())
            .finish()
    }
}

/// Cancels a running link from another thread, set in
/// [`LinkOptions::cancel`].
///
/// Clones share one flag: keep a clone, hand the other to the link, and call
/// [`CancelToken::cancel`] from any thread. The link checks the flag between
/// pipeline stages and inside the long parallel loops (loading inputs,
/// writing the output), and returns the error [`CancelToken::error`] makes;
/// [`CancelToken::is_cancellation`] recognizes it. A cancelled link leaves
/// no output behind: a previous output file at the same path is untouched,
/// and an [`OutputBuffer`] stays empty.
///
/// Cancelling a link that already finished has no effect. A token cannot
/// be reset; use a new one for the next link.
///
/// # Example
///
/// ```
/// use qld::args::{CancelToken, LinkOptions};
///
/// let token = CancelToken::new();
/// let mut options = LinkOptions::new();
/// options.cancel = Some(token.clone());
/// token.cancel(); // from any thread, at any time
/// let error = qld::link(&options, &qld::diag::Collect::new()).unwrap_err();
/// assert!(CancelToken::is_cancellation(&error));
/// ```
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PartialEq for CancelToken {
    /// Two tokens are equal when they are clones of each other.
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }
}

impl Eq for CancelToken {}

impl CancelToken {
    /// Creates a token that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks every link using this token (or a clone) to stop.
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether [`CancelToken::cancel`] was called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns [`CancelToken::error`] if the token was cancelled.
    ///
    /// # Errors
    ///
    /// The cancellation error, once [`CancelToken::cancel`] was called.
    pub fn check(&self) -> crate::Result<()> {
        if self.is_cancelled() {
            Err(Self::error())
        } else {
            Ok(())
        }
    }

    /// The error a cancelled link returns, [`Error::Cancelled`](crate::Error::Cancelled).
    #[must_use]
    pub fn error() -> crate::Error {
        crate::Error::Cancelled
    }

    /// Whether `error` is the error of a cancelled link.
    #[must_use]
    pub fn is_cancellation(error: &crate::Error) -> bool {
        matches!(error, crate::Error::Cancelled)
    }
}

/// Receives text a link would otherwise print, set in
/// [`LinkOptions::map_output`] and [`LinkOptions::timing`].
///
/// A link never writes to the process's standard output or standard error
/// on its own: the two options above are the only text it produces outside
/// its [`DiagnosticSink`](crate::DiagnosticSink), and both are `None` by
/// default, which drops the text. [`LinkOptions::use_process_defaults`]
/// sets them the way the `qld` binary does.
///
/// The callback may run on any thread, including a worker of the link's
/// thread pool, and is called with whole lines or larger pieces.
///
/// # Example
///
/// ```
/// use std::sync::{Arc, Mutex};
/// use qld::args::{LinkOptions, TextOutput};
///
/// let map = Arc::new(Mutex::new(String::new()));
/// let collected = Arc::clone(&map);
/// let mut options = LinkOptions::new();
/// options.print_map = true;
/// options.map_output = Some(TextOutput::new(move |text| {
///     collected.lock().unwrap().push_str(text);
/// }));
/// ```
#[derive(Clone)]
pub struct TextOutput {
    write: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    what: &'static str,
}

impl TextOutput {
    /// Sends the text to `write`.
    #[must_use]
    pub fn new(write: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self {
            write: std::sync::Arc::new(write),
            what: "callback",
        }
    }

    /// Sends the text to the process's standard output, as GNU ld does.
    #[must_use]
    pub fn stdout() -> Self {
        Self {
            write: std::sync::Arc::new(|text: &str| {
                use std::io::Write as _;
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(text.as_bytes());
            }),
            what: "stdout",
        }
    }

    /// Sends the text to the process's standard error.
    #[must_use]
    pub fn stderr() -> Self {
        Self {
            write: std::sync::Arc::new(|text: &str| {
                use std::io::Write as _;
                let mut out = std::io::stderr().lock();
                let _ = out.write_all(text.as_bytes());
            }),
            what: "stderr",
        }
    }

    /// Writes `text`.
    pub fn write(&self, text: &str) {
        (self.write)(text);
    }

    /// Writes `text` followed by a newline.
    pub fn write_line(&self, text: &str) {
        (self.write)(&format!("{text}\n"));
    }
}

impl std::fmt::Debug for TextOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TextOutput").field(&self.what).finish()
    }
}

/// How the output image is held while the link writes it
/// (`QLD_OUTPUT_BACKING`).
///
/// A benchmarking knob: every backing writes the same bytes. `None` in
/// [`LinkOptions::output_backing`] lets the writer choose.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputBacking {
    /// `mmap`: map the output file writable.
    Mapped,
    /// `write`: write the chunks with positional writes.
    Written,
    /// `memory`: build the image in a heap buffer and write it on commit.
    Buffered,
}

impl OutputBacking {
    /// Parses a `QLD_OUTPUT_BACKING` value. `auto` and unknown values give
    /// `None`, which leaves the choice to the writer.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "mmap" | "mapped" => Some(Self::Mapped),
            "write" | "written" | "pwrite" => Some(Self::Written),
            "memory" | "buffer" | "buffered" => Some(Self::Buffered),
            _ => None,
        }
    }
}

/// Everything a link is configured by.
///
/// Construct one with [`LinkOptions::new`] and the builder methods, or parse
/// argv with [`crate::args::parse_gnu`].
///
/// Fields typed `Option<bool>` distinguish "not given" (`None`, meaning the
/// target's or output kind's default applies) from an explicit choice.
///
/// [`LinkOptions::default`] is [`LinkOptions::new`]: both describe the link
/// GNU ld performs when the command line says nothing, so the ten options
/// GNU ld has on by default (`-z relro`, `--demangle`, `--relax`, …) are on
/// in both.
///
/// The struct is `#[non_exhaustive]`: qld adds fields as it implements more
/// options, so build one with [`LinkOptions::new`] and assign the fields you
/// need.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct LinkOptions {
    /// Command-line dialect this was parsed from.
    pub flavor: Flavor,
    /// Target, when it was given explicitly (`-m`). Otherwise it is inferred
    /// from the first object input.
    pub target: Option<Target>,
    /// Endianness forced with `-EB` / `-EL`.
    pub endian: Option<Endianness>,
    /// Output path (`-o`). `None` means the flavor's default (`a.out`).
    pub output: Option<PathBuf>,
    /// Output format (`--oformat`), such as `binary` or `elf64-x86-64`.
    pub output_format: Option<OutputFormat>,
    /// What kind of output to produce.
    pub kind: OutputKind,
    /// Inputs, in command-line order.
    pub inputs: Vec<InputSpec>,
    /// Library search paths (`-L`), in order.
    pub search_paths: Vec<PathBuf>,
    /// `-nostdlib`: ignore `SEARCH_DIR` commands in linker scripts.
    pub nostdlib: bool,
    /// `--sysroot`.
    pub sysroot: Option<PathBuf>,
    /// `--default-script` / `-dT`: used when no `-T` script is given.
    pub default_script: Option<PathBuf>,
    /// Entry point (`-e`).
    pub entry: Option<String>,
    /// `-soname` / `-install_name`.
    pub soname: Option<String>,
    /// Program interpreter (`--dynamic-linker`).
    pub dynamic_linker: Option<PathBuf>,
    /// `--no-dynamic-linker`: omit `PT_INTERP` (used for static PIE).
    pub no_dynamic_linker: bool,
    /// `-rpath` entries, in order.
    pub rpaths: Vec<PathBuf>,
    /// `-rpath-link` entries, in order.
    pub rpath_links: Vec<PathBuf>,
    /// `--enable-new-dtags` (`Some(true)`, `DT_RUNPATH`) or
    /// `--disable-new-dtags` (`Some(false)`, `DT_RPATH`).
    pub new_dtags: Option<bool>,
    /// Symbols required to be defined (`-u`, `--undefined`).
    pub undefined: Vec<String>,
    /// `--undefined-glob` patterns.
    pub undefined_glob: Vec<String>,
    /// `--require-defined` symbols: like `-u`, but an error if undefined.
    pub require_defined: Vec<String>,
    /// `--defsym` assignments, in order.
    pub defsym: Vec<(String, String)>,
    /// `--wrap` symbols.
    pub wrap: Vec<String>,
    /// `-init` symbol (`DT_INIT`).
    pub init: Option<String>,
    /// `-fini` symbol (`DT_FINI`).
    pub fini: Option<String>,
    /// `-f` / `--auxiliary` names (`DT_AUXILIARY`).
    pub auxiliary: Vec<String>,
    /// `-F` / `--filter` names (`DT_FILTER`).
    pub filter: Vec<String>,
    /// Remove unreferenced sections (`--gc-sections`).
    pub gc_sections: bool,
    /// `--print-gc-sections`.
    pub print_gc_sections: bool,
    /// `--gc-keep-exported`: exported symbols are GC roots.
    pub gc_keep_exported: bool,
    /// `--why-live` symbol patterns.
    pub why_live: Vec<String>,
    /// Identical code folding (`--icf`).
    pub icf: IcfMode,
    /// `--print-icf-sections`.
    pub print_icf_sections: bool,
    /// `--keep-unique` symbols, never folded by ICF.
    pub keep_unique: Vec<String>,
    /// `--ignore-data-address-equality`.
    pub ignore_data_address_equality: bool,
    /// `--ignore-function-address-equality`.
    pub ignore_function_address_equality: bool,
    /// Strip level.
    pub strip: StripMode,
    /// Local symbol discarding (`-x`, `-X`, `--discard-none`).
    pub discard: DiscardMode,
    /// `--retain-symbols-file`.
    pub retain_symbols_file: Option<PathBuf>,
    /// Build ID generation.
    pub build_id: BuildId,
    /// Dynamic symbol hash tables to emit.
    pub hash_style: HashStyle,
    /// `--eh-frame-hdr`: create `.eh_frame_hdr` and `PT_GNU_EH_FRAME`.
    pub eh_frame_hdr: bool,
    /// Export all symbols from an executable (`--export-dynamic`).
    pub export_dynamic: bool,
    /// `--export-dynamic-symbol` patterns.
    pub export_dynamic_symbols: Vec<String>,
    /// `--export-dynamic-symbol-list` files.
    pub export_dynamic_symbol_lists: Vec<PathBuf>,
    /// `--dynamic-list` files.
    pub dynamic_lists: Vec<PathBuf>,
    /// `--exclude-libs` archive names (`ALL` is kept as a name).
    pub exclude_libs: Vec<String>,
    /// `--version-script` files.
    pub version_scripts: Vec<PathBuf>,
    /// `--undefined-version` (`Some(true)`) or `--no-undefined-version`
    /// (`Some(false)`).
    pub undefined_version: Option<bool>,
    /// `--default-symver`: version exported symbols with the soname.
    pub default_symver: bool,
    /// `-Bsymbolic` and variants.
    pub symbolic: SymbolicMode,
    /// `--no-undefined` / `-z defs` (`Some(true)`) or `-z undefs`
    /// (`Some(false)`): whether undefined symbols in regular objects are
    /// errors even for shared output.
    pub no_undefined: Option<bool>,
    /// `--allow-shlib-undefined` (`Some(true)`) or
    /// `--no-allow-shlib-undefined` (`Some(false)`).
    pub allow_shlib_undefined: Option<bool>,
    /// `--unresolved-symbols=`.
    pub unresolved_symbols: Option<UnresolvedSymbols>,
    /// `--warn-unresolved-symbols`: report unresolved symbols as warnings.
    pub warn_unresolved_symbols: bool,
    /// `--ignore-unresolved-symbol` names.
    pub ignore_unresolved_symbols: Vec<String>,
    /// `--allow-multiple-definition` / `-z muldefs`.
    pub allow_multiple_definition: bool,
    /// `--warn-common`.
    pub warn_common: bool,
    /// `--warn-backrefs`.
    pub warn_backrefs: bool,
    /// `--warn-backrefs-exclude` patterns.
    pub warn_backrefs_exclude: Vec<String>,
    /// `--warn-textrel`: warn when the output needs `DT_TEXTREL`.
    pub warn_textrel: bool,
    /// `-z text`: make `DT_TEXTREL` an error.
    pub error_textrel: bool,
    /// Resolve all dynamic symbols at load time (`-z now`).
    pub bind_now: bool,
    /// `-z relro` (default) / `-z norelro`.
    pub relro: bool,
    /// `-z separate-code` and friends. `None` means the target default.
    pub separate_code: Option<SeparateCode>,
    /// `--rosegment` (`Some(true)`) / `--no-rosegment` (`Some(false)`).
    pub rosegment: Option<bool>,
    /// `-z execstack` / `-z noexecstack`.
    pub exec_stack: ExecStack,
    /// `-z nognustack` clears this: omit `PT_GNU_STACK`.
    pub gnu_stack: bool,
    /// `-z stack-size=`: the `PT_GNU_STACK` size.
    pub stack_size: Option<u64>,
    /// `-z max-page-size=`.
    pub max_page_size: Option<u64>,
    /// `-z common-page-size=`.
    pub common_page_size: Option<u64>,
    /// `-z copyreloc` (default) / `-z nocopyreloc`.
    pub copy_relocs: bool,
    /// `-z combreloc` (default) / `-z nocombreloc`.
    pub combine_relocs: bool,
    /// `-z pack-relative-relocs` or `--pack-dyn-relocs=relr`: emit `DT_RELR`.
    pub pack_relative_relocs: bool,
    /// `--apply-dynamic-relocs`: also write addends into the output.
    pub apply_dynamic_relocs: bool,
    /// `DT_FLAGS` / `DT_FLAGS_1` bits from `-z` keywords.
    pub dynamic_flags: DynamicFlags,
    /// `-z start-stop-gc` (`Some(true)`) / `-z nostart-stop-gc`.
    pub start_stop_gc: Option<bool>,
    /// `-z start-stop-visibility=`. `None` when the keyword was not given,
    /// which leaves the linker's own default.
    pub start_stop_visibility: Option<Visibility>,
    /// `-z keep-text-section-prefix`.
    pub keep_text_section_prefix: bool,
    /// `-z dynamic-undefined-weak` (`Some(true)`) /
    /// `-z nodynamic-undefined-weak`.
    pub dynamic_undefined_weak: Option<bool>,
    /// `-z noextern-protected-data` clears this.
    pub extern_protected_data: bool,
    /// `-z mark-plt`.
    pub mark_plt: bool,
    /// `-z sectionheader` (default) / `-z nosectionheader`.
    pub section_header: bool,
    /// `-z memory-seal`.
    pub memory_seal: bool,
    /// `-z dead-reloc-in-nonalloc=<glob>=<value>` rules, in command-line
    /// order: the value written for a relocation in a non-allocated section
    /// whose name matches the glob, when its target section was discarded.
    /// Later rules take precedence.
    pub dead_reloc_in_nonalloc: Vec<(String, u64)>,
    /// x86 CET and ISA-level options.
    pub x86: X86Features,
    /// `--fix-cortex-a53-843419`.
    pub fix_cortex_a53_843419: bool,
    /// AArch64 BTI, PAC and erratum options.
    pub aarch64: Aarch64Features,
    /// `--spare-dynamic-tags`.
    pub spare_dynamic_tags: Option<u64>,
    /// `-q` / `--emit-relocs`: keep relocations in the output.
    pub emit_relocs: bool,
    /// `-d`, `-dc`, `-dp`: allocate common symbols even for `-r`.
    pub define_common: bool,
    /// `-n` / `-N`.
    pub magic: MagicMode,
    /// `--relax` (default) / `--no-relax`.
    pub relax: bool,
    /// RISC-V `--relax-gp`: relax absolute accesses within 2 KiB of
    /// `__global_pointer$` to `gp`-relative ones (off by default, as in
    /// lld).
    pub relax_gp: bool,
    /// `--image-base`.
    pub image_base: Option<u64>,
    /// `--section-start`, `-Ttext`, `-Tdata`, `-Tbss`: section addresses, in
    /// order.
    pub section_starts: Vec<(String, u64)>,
    /// `-Ttext-segment`.
    pub text_segment: Option<u64>,
    /// `-Trodata-segment`.
    pub rodata_segment: Option<u64>,
    /// `-Tldata-segment`.
    pub ldata_segment: Option<u64>,
    /// `--orphan-handling=`.
    pub orphan_handling: OrphanHandling,
    /// `--sort-section=`.
    pub sort_section: SortSection,
    /// `--compress-debug-sections=`.
    pub compress_debug_sections: DebugCompression,
    /// `--package-metadata=`: contents of `.note.package`.
    pub package_metadata: Option<String>,
    /// `--symbol-ordering-file`: order input sections by the symbols listed
    /// in this file (lld).
    pub symbol_ordering_file: Option<PathBuf>,
    /// `--no-warn-symbol-ordering`: do not warn about symbols of the
    /// ordering file that cannot be ordered.
    pub no_warn_symbol_ordering: bool,
    /// `--call-graph-profile-sort=`: how to order sections by call graph
    /// profile. `None` when not given: qld then orders only with
    /// `--call-graph-ordering-file` (lld also sorts by default when an
    /// input has a `.llvm.call-graph-profile` section).
    pub call_graph_profile_sort: Option<CallGraphSort>,
    /// `--call-graph-ordering-file`: call graph edges (`from to weight`).
    pub call_graph_ordering_file: Option<PathBuf>,
    /// `--print-symbol-order=`: write the symbol order the call graph sort
    /// chose to this file.
    pub print_symbol_order: Option<PathBuf>,
    /// `--gdb-index`: write a `.gdb_index` section.
    pub gdb_index: bool,
    /// `--s390-pgste`: add an empty `PT_S390_PGSTE` segment, which tells
    /// the Linux kernel to allocate page tables with the guest storage
    /// extension (s390x only).
    pub s390_pgste: bool,
    /// `--debug-names`: write a merged `.debug_names` section.
    pub debug_names: bool,
    /// `--separate-debug-file[=FILE]`: write the debug sections to FILE
    /// (`Some(None)`: the output path plus `.dbg`) and link it from the
    /// output with `.gnu_debuglink`, as mold does.
    pub separate_debug_file: Option<Option<PathBuf>>,
    /// `--dependency-file`.
    pub dependency_file: Option<PathBuf>,
    /// `--dependent-libraries` (default) / `--no-dependent-libraries`.
    pub dependent_libraries: bool,
    /// Optimization level (`-O`).
    pub optimize: u8,
    /// Thread count (`--threads`). `Some(n)` runs the link in a pool of
    /// `n` threads that [`crate::link`] creates for it.
    ///
    /// `None`, the default, leaves the choice to the format driver: the ELF
    /// driver sizes a pool from the input (small links run faster on few
    /// threads), at most 16 threads and at most the available parallelism.
    /// Called inside a rayon pool of your own
    /// ([`rayon::ThreadPool::install`]), a link with `None` runs in that
    /// pool and creates none, however large it is.
    pub threads: Option<usize>,
    /// Write a link map to this path (`-Map`).
    pub map_file: Option<PathBuf>,
    /// `-M` / `--print-map`: write the link map to stdout.
    pub print_map: bool,
    /// `--cref`: add a cross-reference table to the map.
    pub cref: bool,
    /// `-t` / `--trace`: print each input file as it is processed.
    pub trace: bool,
    /// `-y` / `--trace-symbol` names.
    pub trace_symbols: Vec<String>,
    /// `--verbose`.
    pub verbose: bool,
    /// Demangle symbol names in diagnostics (`--demangle`, on by default).
    pub demangle: bool,
    /// `--fatal-warnings`.
    pub fatal_warnings: bool,
    /// `-w` / `--no-warnings`.
    pub no_warnings: bool,
    /// `--error-limit`. `Some(0)` means no limit.
    pub error_limit: Option<u64>,
    /// `--color-diagnostics`.
    pub color: ColorChoice,
    /// `--noinhibit-exec`: write the output even after errors.
    pub noinhibit_exec: bool,
    /// The PE/COFF options of GNU ld's MinGW emulations. Only PE links read
    /// them.
    pub pe: PeArgs,
    /// The Apple ld64 options that have no GNU equivalent. Only Mach-O
    /// links read them.
    pub darwin: crate::args::darwin::DarwinArgs,
    /// LTO plugin paths (`-plugin`) and their options (`-plugin-opt`).
    pub plugins: Vec<(PathBuf, Vec<String>)>,
    /// `-plugin-save-temps`: keep the files an LTO plugin generates.
    pub plugin_save_temps: bool,
    /// Exit the process when an LTO plugin reports a fatal error, as GNU ld
    /// does. The `qld` binary sets this; library callers get an error
    /// instead, and accept that a plugin may not expect to be called again.
    pub exit_on_plugin_fatal: bool,
    /// `--fork` (the default) / `--no-fork`: whether the `qld` binary runs
    /// the link in a child process and returns as soon as the output is
    /// complete, leaving the child to free memory and unmap the inputs.
    /// Only the binary reads it (on Unix); the library never forks.
    pub fork: bool,
    /// Called once when a successful link's output is complete; see
    /// [`OutputCompleteHook`]. `None` by default. The `qld` binary sets it
    /// in the child process of `--fork` to let its parent exit early.
    pub on_output_complete: Option<OutputCompleteHook>,
    /// Files that exist only in memory, looked up by path before the file
    /// system: inputs named by path, `-l` libraries found in the search
    /// directories, `INPUT`/`GROUP` entries of input scripts, and thin
    /// archive members. `None` by default (only the file system). See
    /// [`MemoryFiles`](crate::input::source::MemoryFiles) and
    /// [`InputProvider`](crate::input::source::InputProvider).
    ///
    /// The ELF driver uses it. Files read outside the input list (`-T`
    /// scripts, version scripts, dynamic lists) still come from the file
    /// system; so do all inputs of PE and Mach-O links.
    pub input_provider: Option<std::sync::Arc<dyn crate::input::source::InputProvider>>,
    /// Hand the output image to this buffer instead of writing the output
    /// file; see [`OutputBuffer`]. `None` by default.
    pub output_buffer: Option<OutputBuffer>,
    /// Stop the link with an error once this token is cancelled; see
    /// [`CancelToken`]. `None` by default.
    pub cancel: Option<CancelToken>,
    /// Receives the link map of `-M` / `--print-map`, and the `--cref`
    /// table when no `-Map` file was named. `None` by default, which drops
    /// that text: a library link writes nothing to the process's standard
    /// output. See [`LinkOptions::use_process_defaults`].
    pub map_output: Option<TextOutput>,
    /// Receives one line per pipeline stage with the time it took, the way
    /// `QLD_TIMING` asks the `qld` binary for. `None` by default, which
    /// measures nothing. See [`LinkOptions::use_process_defaults`].
    pub timing: Option<TextOutput>,
    /// `LD_RUN_PATH`, split into directories: searched for the dependencies
    /// of shared libraries when no `-rpath` was given, as GNU ld does.
    /// Empty by default; a library link reads no environment of its own.
    /// See [`LinkOptions::use_process_defaults`].
    pub env_run_path: Vec<PathBuf>,
    /// `LD_LIBRARY_PATH`, split into directories: searched for the
    /// dependencies of shared libraries, as GNU ld does. Empty by default.
    /// See [`LinkOptions::use_process_defaults`].
    pub env_library_path: Vec<PathBuf>,
    /// Record zero modification times in a Mach-O debug map, for
    /// reproducible output; `ZERO_AR_DATE` in the environment asks ld64,
    /// lld and the `qld` binary for it. Off by default. See
    /// [`LinkOptions::use_process_defaults`].
    pub zero_ar_date: bool,
    /// How the output image is held while it is written. `None`, the
    /// default, leaves the choice to the writer; `QLD_OUTPUT_BACKING` sets
    /// it for the `qld` binary. See [`LinkOptions::use_process_defaults`].
    pub output_backing: Option<OutputBacking>,
    /// Options that were recognized but have no effect yet, kept so that
    /// `--verbose` and tests can report them.
    pub ignored: Vec<OsString>,
    /// Warnings produced while parsing the command line (for example an
    /// unknown `-z` keyword), in command-line order. The driver emits them to
    /// its diagnostic sink.
    pub warnings: Vec<String>,
}

impl Default for LinkOptions {
    /// Identical to [`LinkOptions::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl LinkOptions {
    /// Creates the options of a link whose command line said nothing: every
    /// field at the default GNU ld uses.
    #[must_use]
    pub fn new() -> Self {
        Self {
            demangle: true,
            relro: true,
            gnu_stack: true,
            copy_relocs: true,
            combine_relocs: true,
            extern_protected_data: true,
            section_header: true,
            relax: true,
            relax_gp: false,
            dependent_libraries: true,
            fork: true,
            ..Self::blank()
        }
    }

    /// Every field at its own type's default, which is not the same thing:
    /// the options GNU ld has on by default are off here. Only
    /// [`LinkOptions::new`] uses it, and it is private so that no caller can
    /// build the half-off options a derived `Default` would have given.
    ///
    /// A new field has to be listed here, which is where its default is
    /// decided; add it to `new` above as well when GNU ld has it on.
    fn blank() -> Self {
        Self {
            flavor: Default::default(),
            target: Default::default(),
            endian: Default::default(),
            output: Default::default(),
            output_format: Default::default(),
            kind: Default::default(),
            inputs: Default::default(),
            search_paths: Default::default(),
            nostdlib: Default::default(),
            sysroot: Default::default(),
            default_script: Default::default(),
            entry: Default::default(),
            soname: Default::default(),
            dynamic_linker: Default::default(),
            no_dynamic_linker: Default::default(),
            rpaths: Default::default(),
            rpath_links: Default::default(),
            new_dtags: Default::default(),
            undefined: Default::default(),
            undefined_glob: Default::default(),
            require_defined: Default::default(),
            defsym: Default::default(),
            wrap: Default::default(),
            init: Default::default(),
            fini: Default::default(),
            auxiliary: Default::default(),
            filter: Default::default(),
            gc_sections: Default::default(),
            print_gc_sections: Default::default(),
            gc_keep_exported: Default::default(),
            why_live: Default::default(),
            icf: Default::default(),
            print_icf_sections: Default::default(),
            keep_unique: Default::default(),
            ignore_data_address_equality: Default::default(),
            ignore_function_address_equality: Default::default(),
            strip: Default::default(),
            discard: Default::default(),
            retain_symbols_file: Default::default(),
            build_id: Default::default(),
            hash_style: Default::default(),
            eh_frame_hdr: Default::default(),
            export_dynamic: Default::default(),
            export_dynamic_symbols: Default::default(),
            export_dynamic_symbol_lists: Default::default(),
            dynamic_lists: Default::default(),
            exclude_libs: Default::default(),
            version_scripts: Default::default(),
            undefined_version: Default::default(),
            default_symver: Default::default(),
            symbolic: Default::default(),
            no_undefined: Default::default(),
            allow_shlib_undefined: Default::default(),
            unresolved_symbols: Default::default(),
            warn_unresolved_symbols: Default::default(),
            ignore_unresolved_symbols: Default::default(),
            allow_multiple_definition: Default::default(),
            warn_common: Default::default(),
            warn_backrefs: Default::default(),
            warn_backrefs_exclude: Default::default(),
            warn_textrel: Default::default(),
            error_textrel: Default::default(),
            bind_now: Default::default(),
            relro: Default::default(),
            separate_code: Default::default(),
            rosegment: Default::default(),
            exec_stack: Default::default(),
            gnu_stack: Default::default(),
            stack_size: Default::default(),
            max_page_size: Default::default(),
            common_page_size: Default::default(),
            copy_relocs: Default::default(),
            combine_relocs: Default::default(),
            pack_relative_relocs: Default::default(),
            apply_dynamic_relocs: Default::default(),
            dynamic_flags: Default::default(),
            start_stop_gc: Default::default(),
            start_stop_visibility: Default::default(),
            keep_text_section_prefix: Default::default(),
            dynamic_undefined_weak: Default::default(),
            extern_protected_data: Default::default(),
            mark_plt: Default::default(),
            section_header: Default::default(),
            memory_seal: Default::default(),
            dead_reloc_in_nonalloc: Default::default(),
            x86: Default::default(),
            fix_cortex_a53_843419: Default::default(),
            aarch64: Default::default(),
            spare_dynamic_tags: Default::default(),
            emit_relocs: Default::default(),
            define_common: Default::default(),
            magic: Default::default(),
            relax: Default::default(),
            relax_gp: Default::default(),
            image_base: Default::default(),
            section_starts: Default::default(),
            text_segment: Default::default(),
            rodata_segment: Default::default(),
            ldata_segment: Default::default(),
            orphan_handling: Default::default(),
            sort_section: Default::default(),
            compress_debug_sections: Default::default(),
            package_metadata: Default::default(),
            symbol_ordering_file: Default::default(),
            no_warn_symbol_ordering: Default::default(),
            call_graph_profile_sort: Default::default(),
            call_graph_ordering_file: Default::default(),
            print_symbol_order: Default::default(),
            gdb_index: Default::default(),
            s390_pgste: Default::default(),
            debug_names: Default::default(),
            separate_debug_file: Default::default(),
            dependency_file: Default::default(),
            dependent_libraries: Default::default(),
            optimize: Default::default(),
            threads: Default::default(),
            map_file: Default::default(),
            print_map: Default::default(),
            cref: Default::default(),
            trace: Default::default(),
            trace_symbols: Default::default(),
            verbose: Default::default(),
            demangle: Default::default(),
            fatal_warnings: Default::default(),
            no_warnings: Default::default(),
            error_limit: Default::default(),
            color: Default::default(),
            noinhibit_exec: Default::default(),
            pe: Default::default(),
            darwin: Default::default(),
            plugins: Default::default(),
            plugin_save_temps: Default::default(),
            exit_on_plugin_fatal: Default::default(),
            fork: Default::default(),
            on_output_complete: Default::default(),
            input_provider: Default::default(),
            output_buffer: Default::default(),
            cancel: Default::default(),
            map_output: Default::default(),
            timing: Default::default(),
            env_run_path: Default::default(),
            env_library_path: Default::default(),
            zero_ar_date: Default::default(),
            output_backing: Default::default(),
            ignored: Default::default(),
            warnings: Default::default(),
        }
    }

    /// Makes these options describe a link run the way the `qld` binary
    /// runs one, by taking from the process what a library link must be
    /// told explicitly.
    ///
    /// Nothing else in qld reads the environment or writes to standard
    /// output or standard error, so a `LinkOptions` that never went through
    /// this method describes a hermetic, silent link. [`parse_gnu`] and
    /// [`parse_darwin`] call it, because they parse a command line the way
    /// the binary does; [`parse_gnu_with`], [`parse_darwin_with`] and
    /// [`LinkOptions::new`] do not.
    ///
    /// It sets:
    ///
    /// - [`map_output`](Self::map_output) to standard output, where GNU ld
    ///   writes the map of `-M` and a `--cref` table with no `-Map` file;
    /// - [`timing`](Self::timing) to standard error when `QLD_TIMING` is
    ///   set in the environment;
    /// - [`env_run_path`](Self::env_run_path) from `LD_RUN_PATH` and
    ///   [`env_library_path`](Self::env_library_path) from
    ///   `LD_LIBRARY_PATH`, which GNU ld also searches;
    /// - [`zero_ar_date`](Self::zero_ar_date) from `ZERO_AR_DATE`, which
    ///   ld64 and lld also read;
    /// - [`output_backing`](Self::output_backing) from
    ///   `QLD_OUTPUT_BACKING`, a benchmarking knob.
    ///
    /// [`parse_gnu`]: crate::args::parse_gnu
    /// [`parse_darwin`]: crate::args::parse_darwin
    /// [`parse_gnu_with`]: crate::args::parse_gnu_with
    /// [`parse_darwin_with`]: crate::args::parse_darwin_with
    pub fn use_process_defaults(&mut self) {
        self.map_output = Some(TextOutput::stdout());
        if std::env::var_os("QLD_TIMING").is_some() {
            self.timing = Some(TextOutput::stderr());
        }
        if let Some(run_path) = std::env::var_os("LD_RUN_PATH") {
            self.env_run_path = std::env::split_paths(&run_path).collect();
        }
        if let Some(library_path) = std::env::var_os("LD_LIBRARY_PATH") {
            self.env_library_path = std::env::split_paths(&library_path).collect();
        }
        self.zero_ar_date =
            std::env::var_os("ZERO_AR_DATE").is_some_and(|v| !v.is_empty() && v != "0");
        self.output_backing = std::env::var("QLD_OUTPUT_BACKING")
            .ok()
            .and_then(|value| OutputBacking::from_name(value.trim()));
    }

    /// Writes `text` to [`map_output`](Self::map_output), if there is one.
    pub(crate) fn print_text(&self, text: &str) {
        if let Some(output) = &self.map_output {
            output.write(text);
        }
    }

    /// Runs the [`on_output_complete`](Self::on_output_complete) hook, if
    /// there is one and it has not run yet.
    ///
    /// Link drivers call this once the output of a successful link is
    /// complete, and [`crate::link`] calls it before returning `Ok` for
    /// drivers that did not.
    pub fn output_complete(&self) {
        if let Some(hook) = &self.on_output_complete {
            hook.call();
        }
    }

    /// Returns an error if the link was cancelled through
    /// [`LinkOptions::cancel`]. Link drivers call this between stages.
    ///
    /// # Errors
    ///
    /// [`CancelToken::error`] once the token is cancelled.
    #[inline]
    pub fn check_cancelled(&self) -> crate::Result<()> {
        match &self.cancel {
            Some(token) => token.check(),
            None => Ok(()),
        }
    }

    /// Returns the output path, or the flavor's default.
    #[must_use]
    pub fn output_path(&self) -> PathBuf {
        self.output
            .clone()
            .unwrap_or_else(|| PathBuf::from("a.out"))
    }

    /// Whether the output is position-independent.
    #[must_use]
    pub fn is_pic(&self) -> bool {
        matches!(
            self.kind,
            OutputKind::Pie | OutputKind::Shared | OutputKind::StaticPie
        )
    }

    /// Whether the output links against shared libraries at run time.
    #[must_use]
    pub fn is_dynamic(&self) -> bool {
        matches!(
            self.kind,
            OutputKind::Executable | OutputKind::Pie | OutputKind::Shared
        )
    }

    /// Appends an input with the given attributes.
    pub fn push_input(&mut self, kind: InputKind, attrs: InputAttrs) {
        let position = self.inputs.len();
        self.inputs.push(InputSpec {
            kind,
            attrs,
            position,
        });
    }

    /// Resolves a path that may start with `=` or `$SYSROOT` against
    /// [`LinkOptions::sysroot`], as GNU ld does for `-L`, `-T`, `-rpath-link`
    /// and script `INPUT`s.
    ///
    /// Paths without the prefix are returned unchanged. When there is no
    /// sysroot the prefix is simply removed. This is a pure string operation;
    /// it does not touch the file system.
    #[must_use]
    pub fn resolve_sysroot(&self, path: &Path) -> PathBuf {
        match sysroot_relative(path) {
            Some(rest) => match &self.sysroot {
                Some(root) => {
                    let mut joined = root.clone().into_os_string();
                    joined.push(rest);
                    PathBuf::from(joined)
                }
                None => PathBuf::from(rest),
            },
            None => path.to_path_buf(),
        }
    }
}

/// If `path` starts with `=` or `$SYSROOT`, returns the rest of it (which
/// normally starts with a separator).
#[must_use]
pub fn sysroot_relative(path: &Path) -> Option<&std::ffi::OsStr> {
    let bytes = path.as_os_str().as_encoded_bytes();
    let prefix_len = if bytes.starts_with(b"=") {
        1
    } else if bytes.starts_with(b"$SYSROOT") {
        8
    } else {
        return None;
    };
    // The prefix is ASCII, so splitting right after it is on a valid
    // boundary. Go through `to_str` (and a lossless fallback on Unix) so no
    // unsafe conversion is needed.
    split_after_ascii_prefix(path.as_os_str(), prefix_len)
}

#[cfg(unix)]
fn split_after_ascii_prefix(s: &std::ffi::OsStr, len: usize) -> Option<&std::ffi::OsStr> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().get(len..).map(std::ffi::OsStr::from_bytes)
}

#[cfg(not(unix))]
fn split_after_ascii_prefix(s: &std::ffi::OsStr, len: usize) -> Option<&std::ffi::OsStr> {
    s.to_str()
        .and_then(|s| s.get(len..))
        .map(std::ffi::OsStr::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_gnu_ld() {
        let options = LinkOptions::new();
        assert_eq!(options.output_path(), PathBuf::from("a.out"));
        assert_eq!(options.kind, OutputKind::Executable);
        assert!(!options.gc_sections);
        assert!(options.demangle);
        assert!(options.relro);
        assert!(options.relax);
        assert!(options.is_dynamic());
        assert!(!options.is_pic());
    }

    /// `Default` must not be a second, half-configured constructor: a
    /// caller who writes `LinkOptions::default()` gets the same link as one
    /// who writes `LinkOptions::new()`.
    #[test]
    fn default_is_new() {
        assert_eq!(
            format!("{:?}", LinkOptions::default()),
            format!("{:?}", LinkOptions::new())
        );
        let options = LinkOptions::default();
        assert!(options.relro);
        assert!(options.demangle);
        assert!(options.relax);
        assert!(options.gnu_stack);
        assert!(options.copy_relocs);
        assert!(options.combine_relocs);
        assert!(options.extern_protected_data);
        assert!(options.section_header);
        assert!(options.dependent_libraries);
        assert!(options.fork);
    }

    #[test]
    fn inputs_keep_command_line_positions() {
        let mut options = LinkOptions::new();
        options.push_input(InputKind::File("a.o".into()), InputAttrs::default());
        options.push_input(InputKind::Library("c".into()), InputAttrs::default());
        assert_eq!(options.inputs[1].position, 1);
    }

    #[test]
    fn sysroot_prefixes_resolve_without_io() {
        let mut options = LinkOptions::new();
        assert_eq!(
            options.resolve_sysroot(Path::new("=/usr/lib")),
            PathBuf::from("/usr/lib")
        );
        options.sysroot = Some("/sys".into());
        assert_eq!(
            options.resolve_sysroot(Path::new("=/usr/lib")),
            PathBuf::from("/sys/usr/lib")
        );
        assert_eq!(
            options.resolve_sysroot(Path::new("$SYSROOT/lib")),
            PathBuf::from("/sys/lib")
        );
        assert_eq!(
            options.resolve_sysroot(Path::new("/lib")),
            PathBuf::from("/lib")
        );
    }
}
