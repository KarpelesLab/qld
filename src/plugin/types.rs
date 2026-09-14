//! Plugin data as safe Rust values.

// The conversions to and from the C interface are used by the Unix host only.
#![cfg_attr(not(unix), allow(dead_code))]

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

/// The kind of output the link produces, as reported to plugins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum OutputKind {
    /// Relocatable output (`-r`).
    Relocatable,
    /// A position-dependent executable, static or dynamic.
    #[default]
    Executable,
    /// A shared library (`-shared`).
    SharedObject,
    /// A position-independent executable (`-pie`, `-static-pie`).
    Pie,
}

impl OutputKind {
    /// The integer the plugin interface uses for this kind.
    pub(crate) const fn raw(self) -> i32 {
        match self {
            Self::Relocatable => 0,
            Self::Executable => 1,
            Self::SharedObject => 2,
            Self::Pie => 3,
        }
    }
}

impl From<crate::args::OutputKind> for OutputKind {
    fn from(kind: crate::args::OutputKind) -> Self {
        use crate::args::OutputKind as Link;
        match kind {
            Link::Executable | Link::StaticExecutable => Self::Executable,
            Link::Pie | Link::StaticPie => Self::Pie,
            Link::Shared => Self::SharedObject,
            Link::Relocatable => Self::Relocatable,
        }
    }
}

/// Whether a claimed symbol is a definition or a reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolKind {
    /// A strong definition.
    Definition,
    /// A weak definition.
    WeakDefinition,
    /// A strong reference.
    Undefined,
    /// A weak reference.
    WeakUndefined,
    /// A tentative (common) definition; [`ClaimedSymbol::size`] is its size.
    Common,
}

impl SymbolKind {
    pub(crate) const fn from_raw(raw: u8) -> Option<Self> {
        Some(match raw {
            0 => Self::Definition,
            1 => Self::WeakDefinition,
            2 => Self::Undefined,
            3 => Self::WeakUndefined,
            4 => Self::Common,
            _ => return None,
        })
    }

    /// `true` for the two reference kinds.
    #[must_use]
    pub const fn is_undefined(self) -> bool {
        matches!(self, Self::Undefined | Self::WeakUndefined)
    }
}

/// Symbol visibility, with ELF's meaning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Visibility {
    /// `STV_DEFAULT`.
    #[default]
    Default,
    /// `STV_PROTECTED`.
    Protected,
    /// `STV_INTERNAL`.
    Internal,
    /// `STV_HIDDEN`.
    Hidden,
}

impl Visibility {
    pub(crate) fn from_raw(raw: i32) -> Option<Self> {
        Some(match raw {
            0 => Self::Default,
            1 => Self::Protected,
            2 => Self::Internal,
            3 => Self::Hidden,
            _ => return None,
        })
    }
}

/// What a symbol names, when the plugin says (`add_symbols_v2` only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolType {
    /// Not reported.
    #[default]
    Unknown,
    /// Code.
    Function,
    /// Data.
    Variable,
}

/// Where a definition will live, when the plugin says (`add_symbols_v2` only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SectionKind {
    /// Initialized data or code, or not reported.
    #[default]
    Default,
    /// Zero-initialized data (`.bss`).
    Bss,
}

/// A symbol of a claimed IR file, as the plugin reported it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClaimedSymbol {
    /// Name bytes, without a version suffix. Not necessarily UTF-8.
    pub name: Vec<u8>,
    /// Symbol version, when the plugin gave one.
    pub version: Option<Vec<u8>>,
    /// Definition or reference, weak or not, or common.
    pub kind: SymbolKind,
    /// Visibility.
    pub visibility: Visibility,
    /// Size; meaningful for common symbols.
    pub size: u64,
    /// COMDAT group key: definitions sharing a key are copies of each other,
    /// and only one file's copies may prevail.
    pub comdat_key: Option<Vec<u8>>,
    /// Function or data, when known.
    pub symbol_type: SymbolType,
    /// Default or BSS placement, when known.
    pub section_kind: SectionKind,
}

/// How the linker resolved one claimed symbol, reported back to the plugin
/// before it compiles.
///
/// Definitions get one of the `Prevailing*` or `Preempted*` values;
/// references get one of the `Resolved*` values or [`Undefined`](Self::Undefined).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolResolution {
    /// Not resolved. Plugins treat this as a linker bug; never report it.
    Unknown,
    /// The reference is still undefined.
    Undefined,
    /// This definition prevails and is referenced from a regular object (or
    /// must be kept visible for another reason, such as `-r` or `--wrap`).
    PrevailingDef,
    /// This definition prevails and only IR refers to it: the plugin may
    /// internalize or discard it.
    PrevailingDefIronly,
    /// A regular object's definition preempts this one.
    PreemptedRegular,
    /// Another IR file's definition preempts this one.
    PreemptedIr,
    /// The reference resolves to a definition in another IR file.
    ResolvedIr,
    /// The reference resolves to a regular object linked into the output.
    ResolvedExec,
    /// The reference resolves to a shared library.
    ResolvedDyn,
    /// Like [`PrevailingDefIronly`](Self::PrevailingDefIronly), but the symbol
    /// is exported from the output, so a shared object the linker cannot see
    /// may refer to it. Plugins using the first `get_symbols` version see
    /// [`PrevailingDef`](Self::PrevailingDef) instead.
    PrevailingDefIronlyExp,
}

impl SymbolResolution {
    /// The integer the plugin interface uses.
    pub(crate) const fn raw(self) -> i32 {
        match self {
            Self::Unknown => 0,
            Self::Undefined => 1,
            Self::PrevailingDef => 2,
            Self::PrevailingDefIronly => 3,
            Self::PreemptedRegular => 4,
            Self::PreemptedIr => 5,
            Self::ResolvedIr => 6,
            Self::ResolvedExec => 7,
            Self::ResolvedDyn => 8,
            Self::PrevailingDefIronlyExp => 9,
        }
    }
}

/// The resolution of every symbol of one claimed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileResolution {
    /// The file is part of the link. One entry per
    /// [`ClaimedFile::symbols`] entry, in the same order.
    Included(Vec<SymbolResolution>),
    /// The file was claimed but is not part of the link: an archive member
    /// that no round of resolution extracted. The plugin sees its definitions
    /// as preempted (or, through `get_symbols_v3`, no symbols at all) and does
    /// not compile it.
    NotIncluded,
}

/// A file a plugin claimed, with the symbols it reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimedFile {
    /// The caller's handle from [`InputFile::handle`].
    pub handle: u64,
    /// The file's path (the archive, for a member).
    pub path: PathBuf,
    /// Offset of the IR within `path`.
    pub offset: u64,
    /// Size of the IR.
    pub size: u64,
    /// Index of the claiming plugin, in load order.
    pub plugin: usize,
    /// The symbols, in the plugin's order.
    pub symbols: Vec<ClaimedSymbol>,
}

/// A file offered to the plugins in [`Session::claim`](super::Session::claim).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputFile {
    /// Path of the file. For an archive member, the archive's path, with the
    /// member located by `offset` and `size`, the way GNU ld reports members.
    pub path: PathBuf,
    /// Offset of the object within `path`: 0 for a plain file.
    pub offset: u64,
    /// Size of the object in bytes.
    pub size: u64,
    /// Any value that identifies the file to the caller, such as its
    /// [`FileId`](crate::FileId) index. It is returned in [`ClaimedFile`] and
    /// [`SectionRef`], and is never interpreted.
    pub handle: u64,
    /// `false` when the linker is only looking at an archive member to learn
    /// what it defines, and has not decided to include it. Plugins with a
    /// second-version claim handler may claim such a file differently. Plain
    /// files on the command line are known to be used.
    pub known_used: bool,
}

impl InputFile {
    /// Describes `size` bytes at `offset` in `path`, known to be used.
    pub fn new(path: impl Into<PathBuf>, offset: u64, size: u64, handle: u64) -> Self {
        Self {
            path: path.into(),
            offset,
            size,
            handle,
            known_used: true,
        }
    }
}

/// Severity of a plugin message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MessageLevel {
    /// Informational output.
    Info,
    /// A warning.
    Warning,
    /// An error: the link fails, but the plugin continues.
    Error,
    /// A fatal error: the plugin cannot continue, and the session is unusable.
    Fatal,
}

impl MessageLevel {
    /// Unknown levels count as errors, as GNU ld treats them.
    pub(crate) fn from_raw(raw: i32) -> Self {
        match raw {
            0 => Self::Info,
            1 => Self::Warning,
            3 => Self::Fatal,
            _ => Self::Error,
        }
    }
}

/// A message a plugin sent through the `message` callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginMessage {
    /// Severity.
    pub level: MessageLevel,
    /// The formatted text, without a trailing newline.
    pub text: String,
}

impl fmt::Display for PluginMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// A section of an input file, named by the caller's handle and its ELF
/// section index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SectionRef {
    /// [`InputFile::handle`] of the file.
    pub file: u64,
    /// Section header index.
    pub index: u32,
}

/// A request to place sections in a segment of their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniqueSegment {
    /// Name of the output section to create for them.
    pub name: Vec<u8>,
    /// Extra `p_flags` bits for the segment.
    pub flags: u64,
    /// Segment alignment; 0 for the default.
    pub alignment: u64,
    /// The input sections, in order.
    pub sections: Vec<SectionRef>,
}

/// Everything the plugins asked of the linker during
/// [`Session::all_symbols_read`](super::Session::all_symbols_read).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LtoOutput {
    /// Native object files to add to the link (`add_input_file`), in order.
    /// They replace the claimed IR files. Plugins usually delete them in
    /// their cleanup handlers, so read them before the session ends.
    pub files: Vec<PathBuf>,
    /// Libraries to search, as `-l` names without the `-l` (`add_input_library`).
    pub libraries: Vec<OsString>,
    /// Directories to search for those libraries (`set_extra_library_path`).
    pub library_paths: Vec<PathBuf>,
    /// The section order a plugin asked for (`update_section_order`), if any.
    pub section_order: Option<Vec<SectionRef>>,
    /// Unique-segment requests (`unique_segment_for_sections`).
    pub unique_segments: Vec<UniqueSegment>,
    /// Number of error-level messages the plugins reported. The link must
    /// fail if this is not zero, although the plugins did not stop.
    pub errors: usize,
}
