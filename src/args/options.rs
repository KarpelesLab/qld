//! The option model: everything a link is configured by.
//!
//! This is the interface between the argv front ends, the library API and the
//! link driver. Adding a field here is how a new option becomes visible to the
//! rest of qld.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::target::Target;

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StripMode {
    /// Keep everything (default).
    #[default]
    None,
    /// Drop debug sections (`-S`, `--strip-debug`).
    Debug,
    /// Drop debug sections and the symbol table (`-s`, `--strip-all`).
    All,
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
#[derive(Clone, Debug, Default)]
pub struct LinkOptions {
    /// Command-line dialect this was parsed from.
    pub flavor: Flavor,
    /// Target, when it was given explicitly (`-m`). Otherwise it is inferred
    /// from the first object input.
    pub target: Option<Target>,
    /// Output path (`-o`). `None` means the flavor's default (`a.out`).
    pub output: Option<PathBuf>,
    /// What kind of output to produce.
    pub kind: OutputKind,
    /// Inputs, in command-line order.
    pub inputs: Vec<InputSpec>,
    /// Library search paths (`-L`), in order.
    pub search_paths: Vec<PathBuf>,
    /// `--sysroot`.
    pub sysroot: Option<PathBuf>,
    /// Entry point (`-e`).
    pub entry: Option<String>,
    /// `-soname` / `-install_name`.
    pub soname: Option<String>,
    /// Program interpreter (`--dynamic-linker`).
    pub dynamic_linker: Option<PathBuf>,
    /// `-rpath` entries, in order.
    pub rpaths: Vec<PathBuf>,
    /// Symbols required to be defined (`-u`, `--undefined`).
    pub undefined: Vec<String>,
    /// `--defsym` assignments, in order.
    pub defsym: Vec<(String, String)>,
    /// `--wrap` symbols.
    pub wrap: Vec<String>,
    /// Remove unreferenced sections (`--gc-sections`).
    pub gc_sections: bool,
    /// Identical code folding (`--icf`), as the raw keyword for now.
    pub icf: Option<String>,
    /// Strip level.
    pub strip: StripMode,
    /// Build ID generation.
    pub build_id: BuildId,
    /// Dynamic symbol hash tables to emit.
    pub hash_style: HashStyle,
    /// Export all symbols from an executable (`--export-dynamic`).
    pub export_dynamic: bool,
    /// Resolve all dynamic symbols at load time (`-z now`).
    pub bind_now: bool,
    /// Optimization level (`-O`).
    pub optimize: u8,
    /// Thread count. `None` means one thread per available core.
    pub threads: Option<usize>,
    /// Write a link map to this path (`-Map`).
    pub map_file: Option<PathBuf>,
    /// Demangle symbol names in diagnostics (`--demangle`, on by default).
    pub demangle: bool,
    /// LTO plugin paths (`-plugin`) and their options (`-plugin-opt`).
    pub plugins: Vec<(PathBuf, Vec<String>)>,
    /// Options that were recognized but have no effect yet, kept so that
    /// `--verbose` and tests can report them.
    pub ignored: Vec<OsString>,
}

impl LinkOptions {
    /// Creates options with every field at its default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            demangle: true,
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
}
