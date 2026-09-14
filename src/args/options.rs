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

/// Dynamic section flags set by `-z` keywords (`DT_FLAGS` / `DT_FLAGS_1`).
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

/// Severity for the `-z *-report=` keywords.
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

/// How the inputs that follow `-b` / `--format` are interpreted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputAttrs {
    /// `--whole-archive` was in effect.
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
}

/// What an input entry refers to.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// A file named directly on the command line.
    File(PathBuf),
    /// `-lfoo`: search for `libfoo.so` and `libfoo.a` in the search paths.
    Library(String),
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

/// One input, with the positional state that applied to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputSpec {
    /// What the input is.
    pub kind: InputKind,
    /// Positional attributes in effect at this point in the command line.
    pub attrs: InputAttrs,
    /// Zero-based position on the command line. Used for precedence decisions
    /// and to order diagnostics deterministically.
    pub position: usize,
}

/// Everything a link is configured by.
///
/// Construct one with [`LinkOptions::new`] and the builder methods, or parse
/// argv with [`crate::args::parse_gnu`].
///
/// Fields typed `Option<bool>` distinguish "not given" (`None`, meaning the
/// target's or output kind's default applies) from an explicit choice.
#[derive(Clone, Debug, Default)]
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
    /// Output format name (`--oformat`), such as `binary` or `elf64-x86-64`.
    pub output_format: Option<String>,
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
    /// Identical code folding (`--icf`): `"all"` or `"safe"`. `None` means
    /// no folding (`--icf=none`, the default).
    pub icf: Option<String>,
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
    /// `-z start-stop-visibility=`: `default`, `internal`, `hidden` or
    /// `protected`.
    pub start_stop_visibility: Option<String>,
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
    /// `--orphan-handling=`: `place`, `warn`, `error` or `discard`.
    pub orphan_handling: Option<String>,
    /// `--sort-section=`: `name` or `alignment`.
    pub sort_section: Option<String>,
    /// `--compress-debug-sections=`: `none`, `zlib`, `zlib-gnu`,
    /// `zlib-gabi` or `zstd`.
    pub compress_debug_sections: Option<String>,
    /// `--package-metadata=`: contents of `.note.package`.
    pub package_metadata: Option<String>,
    /// `--dependency-file`.
    pub dependency_file: Option<PathBuf>,
    /// `--dependent-libraries` (default) / `--no-dependent-libraries`.
    pub dependent_libraries: bool,
    /// Optimization level (`-O`).
    pub optimize: u8,
    /// Thread count. `None` means one thread per available core.
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
    /// LTO plugin paths (`-plugin`) and their options (`-plugin-opt`).
    pub plugins: Vec<(PathBuf, Vec<String>)>,
    /// `-plugin-save-temps`: keep the files an LTO plugin generates.
    pub plugin_save_temps: bool,
    /// Exit the process when an LTO plugin reports a fatal error, as GNU ld
    /// does. The `qld` binary sets this; library callers get an error
    /// instead, and accept that a plugin may not expect to be called again.
    pub exit_on_plugin_fatal: bool,
    /// Options that were recognized but have no effect yet, kept so that
    /// `--verbose` and tests can report them.
    pub ignored: Vec<OsString>,
    /// Warnings produced while parsing the command line (for example an
    /// unknown `-z` keyword), in command-line order. The driver emits them to
    /// its diagnostic sink.
    pub warnings: Vec<String>,
}

impl LinkOptions {
    /// Creates options with every field at its default.
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
            dependent_libraries: true,
            ..Self::default()
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
