//! Hints for undefined and duplicate symbols ("intelligent library symbol
//! matching").
//!
//! **Workstream W13.** Given an undefined symbol and the link's search paths,
//! suggests the library that defines it (`did you forget -lm?`), explains
//! version mismatches (`memcpy@GLIBC_2.14` versus the versions available), and
//! proposes near-miss names (C/C++ linkage mismatches, leading underscores,
//! namespace or qualifier differences). The index of search-path libraries is
//! built lazily, only after a link has already failed. See
//! `docs/optimizations.md` ("Intelligent library symbol matching").
//!
//! # Use
//!
//! ```no_run
//! use qld::hints::{Hinter, LinkedLibrary, SearchScope, Undefined};
//! # let options = qld::LinkOptions::default();
//! # let defined: Vec<&[u8]> = Vec::new();
//! let hinter = Hinter::new(SearchScope::from_options(&options), Vec::new());
//! let undefined = [Undefined::new(b"cos")];
//! let hints = hinter.hints(&undefined, &defined);
//! let diagnostic = qld::hints::attach(
//!     qld::Diagnostic::error("undefined symbol: cos"),
//!     &hints[0],
//!     options.demangle,
//! );
//! ```
//!
//! renders (through the CLI's diagnostic sink) as
//!
//! ```text
//! qld: error: undefined symbol: cos
//! >>> note: 'cos' is defined in libm.so.6 (/usr/lib64/libm.so); did you forget -lm?
//! ```
//!
//! Everything here runs after a link has failed, so it favors clear answers
//! over speed, but it stays quick: the library scan maps each distinct file
//! once, in parallel, and records only the names asked about.
//!
//! Results are deterministic: libraries are ranked by search order, then
//! shared before static, then by path; near misses by kind, then by name.

mod library;
mod near;

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::args::LinkOptions;
use crate::diag::Diagnostic;
use crate::input::search::apply_sysroot;

pub use library::{Definition, Entry, LibraryIndex, LibraryKind, Object};
pub use near::{MAX_NEAR_MISSES, NearMiss, NearMissKind, near_misses};

/// At most this many libraries are suggested per undefined symbol.
pub const MAX_LIBRARIES: usize = 3;

/// An undefined symbol to explain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Undefined<'a> {
    /// The symbol name, as mangled.
    pub name: &'a [u8],
    /// The version the reference asks for (`GLIBC_2.14`), if any.
    pub version: Option<&'a [u8]>,
}

impl<'a> Undefined<'a> {
    /// An unversioned reference.
    #[must_use]
    pub fn new(name: &'a [u8]) -> Self {
        Self {
            name,
            version: None,
        }
    }

    /// A reference to `name@version`.
    #[must_use]
    pub fn versioned(name: &'a [u8], version: &'a [u8]) -> Self {
        Self {
            name,
            version: Some(version),
        }
    }

    /// Splits `name@version` (or `name@@version`) into name and version.
    #[must_use]
    pub fn parse(text: &'a [u8]) -> Self {
        match text.iter().position(|&c| c == b'@') {
            Some(at) => {
                let name = text.get(..at).unwrap_or_default();
                let rest = text.get(at.saturating_add(1)..).unwrap_or_default();
                let version = rest.strip_prefix(b"@").unwrap_or(rest);
                Self {
                    name,
                    version: (!version.is_empty()).then_some(version),
                }
            }
            None => Self::new(text),
        }
    }
}

/// A library the link already uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedLibrary {
    /// The file that was loaded (a `-l` resolved to its path).
    pub path: PathBuf,
    /// `--as-needed` dropped it: nothing referenced it when it was loaded.
    pub dropped_as_needed: bool,
    /// It was found with `-Bstatic` (or `-static`) in effect, so `-l`
    /// considered only archives.
    pub static_only: bool,
}

impl LinkedLibrary {
    /// A library that was linked normally.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            dropped_as_needed: false,
            static_only: false,
        }
    }
}

/// Where libraries are searched for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchScope {
    /// Search directories, in order (`-L`, then the defaults).
    pub search_paths: Vec<PathBuf>,
    /// The sysroot, if any.
    pub sysroot: Option<PathBuf>,
}

impl SearchScope {
    /// The search directories and sysroot of a link.
    #[must_use]
    pub fn from_options(options: &LinkOptions) -> Self {
        Self {
            search_paths: options.search_paths.clone(),
            sysroot: options.sysroot.clone(),
        }
    }

    /// The search directories with sysroot prefixes applied and duplicates
    /// removed, in order.
    #[must_use]
    pub fn directories(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for dir in &self.search_paths {
            let dir = apply_sysroot(dir, self.sysroot.as_deref());
            if !out.contains(&dir) {
                out.push(dir);
            }
        }
        out
    }
}

/// A library that defines an undefined symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryMatch {
    /// The file defining the symbol, as diagnostics name it: a shared
    /// object's `DT_SONAME` (`libm.so.6`) or the file name, with the archive
    /// member for archives (`libfoo.a(foo.o)`).
    pub object: String,
    /// The file `-l` finds (`/usr/lib64/libm.so`, possibly a linker script
    /// that pulls in `object`).
    pub path: PathBuf,
    /// The flag that links it: `-lm`, `-l:libfoo.so.1`, or the path.
    pub flag: String,
    /// Shared object or archive.
    pub kind: LibraryKind,
    /// The version of the definition, for versioned shared objects.
    pub version: Option<String>,
}

/// One version of a symbol a library provides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableVersion {
    /// The library.
    pub library: LibraryMatch,
    /// Whether this is the default version (`foo@@V`).
    pub default: bool,
}

/// A hint about an undefined symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Hint {
    /// A library that is not linked defines the symbol.
    MissingLibrary {
        /// The undefined symbol.
        symbol: Vec<u8>,
        /// The library.
        library: LibraryMatch,
    },
    /// A linked library defines the symbol, but `--as-needed` dropped it.
    DroppedAsNeeded {
        /// The undefined symbol.
        symbol: Vec<u8>,
        /// The library.
        library: LibraryMatch,
    },
    /// A shared library defines the symbol, but `-l` found the archive of
    /// the same name because of `-Bstatic`, and the archive does not.
    StaticOnly {
        /// The undefined symbol.
        symbol: Vec<u8>,
        /// The shared library that defines it.
        library: LibraryMatch,
        /// The archive that was linked instead.
        linked: PathBuf,
    },
    /// Libraries define the symbol, but not at the version the reference
    /// asks for (or, for an unversioned reference, only at hidden versions).
    VersionMismatch {
        /// The undefined symbol.
        symbol: Vec<u8>,
        /// The version asked for.
        wanted: Option<String>,
        /// The versions available, by library.
        available: Vec<AvailableVersion>,
    },
    /// A defined name the reference may have meant.
    NearMiss {
        /// The undefined symbol.
        symbol: Vec<u8>,
        /// The candidate.
        near: NearMiss,
    },
}

/// Computes hints for undefined symbols.
///
/// Creating a `Hinter` is free; the library index is built by
/// [`Hinter::hints`], so only links that fail pay for it.
#[derive(Clone, Debug)]
pub struct Hinter {
    scope: SearchScope,
    linked: Vec<LinkedLibrary>,
}

impl Hinter {
    /// A hinter for a link with these search directories and libraries.
    #[must_use]
    pub fn new(scope: SearchScope, linked: Vec<LinkedLibrary>) -> Self {
        Self { scope, linked }
    }

    /// Hints for each of `undefined`, in the same order: libraries first
    /// (missing, dropped, static-only, version mismatches), then near misses
    /// among `defined`, the names the link defines.
    #[must_use]
    pub fn hints(&self, undefined: &[Undefined<'_>], defined: &[&[u8]]) -> Vec<Vec<Hint>> {
        let mut out = self.library_hints(undefined);
        let names: Vec<&[u8]> = undefined.iter().map(|u| u.name).collect();
        for ((hints, symbol), near) in out.iter_mut().zip(&names).zip(near_misses(&names, defined))
        {
            hints.extend(near.into_iter().map(|near| Hint::NearMiss {
                symbol: symbol.to_vec(),
                near,
            }));
        }
        out
    }

    /// Library hints only: builds the index of the search directories and
    /// looks each symbol up.
    #[must_use]
    pub fn library_hints(&self, undefined: &[Undefined<'_>]) -> Vec<Vec<Hint>> {
        if undefined.is_empty() {
            return Vec::new();
        }
        let names: Vec<&[u8]> = undefined.iter().map(|u| u.name).collect();
        let index = LibraryIndex::build(&self.scope, &self.linked, &names);
        undefined
            .iter()
            .map(|symbol| library_hints_for(&self.scope, &index, symbol))
            .collect()
    }
}

/// Whether a definition can satisfy a reference asking for `wanted`.
fn version_matches(definition: &Definition, wanted: Option<&[u8]>) -> bool {
    match wanted {
        Some(wanted) => definition.version.as_deref().map(str::as_bytes) == Some(wanted),
        None => !definition.hidden,
    }
}

fn library_match(
    scope: &SearchScope,
    index: &LibraryIndex,
    definition: &Definition,
) -> Option<LibraryMatch> {
    let object = index.objects().get(definition.object)?;
    let kind = object.kind?;
    let entry = index.best_entry(definition.object);
    let (path, flag) = match entry {
        Some(entry) => (entry.path.clone(), LibraryIndex::flag(scope, entry)),
        None => (object.path.clone(), object.path.display().to_string()),
    };
    let display = match &definition.member {
        Some(member) => format!("{}({member})", object.display),
        None => object.display.clone(),
    };
    Some(LibraryMatch {
        object: display,
        path,
        flag,
        kind,
        version: definition.version.clone(),
    })
}

fn library_hints_for(
    scope: &SearchScope,
    index: &LibraryIndex,
    symbol: &Undefined<'_>,
) -> Vec<Hint> {
    let definitions = index.definitions(symbol.name);
    let mut hints = Vec::new();
    let mut seen_objects: Vec<usize> = Vec::new();
    let mut candidates: Vec<(LibraryMatch, usize, Option<&LinkedLibrary>)> = Vec::new();
    for definition in definitions {
        if !version_matches(definition, symbol.version) || seen_objects.contains(&definition.object)
        {
            continue;
        }
        seen_objects.push(definition.object);
        let Some(library) = library_match(scope, index, definition) else {
            continue;
        };
        let dir = index
            .best_entry(definition.object)
            .map_or(usize::MAX, |entry| entry.dir);
        candidates.push((library, dir, index.linked(definition.object)));
    }
    candidates.sort_by(|a, b| {
        (a.1, a.0.kind, &a.0.path, &a.0.object).cmp(&(b.1, b.0.kind, &b.0.path, &b.0.object))
    });
    // One suggestion per library name: `libm.so` makes `libm.a` redundant.
    let mut stems: Vec<String> = Vec::new();
    candidates.retain(|(library, _, _)| {
        let stem = library_stem(&library.path);
        if stems.contains(&stem) {
            false
        } else {
            stems.push(stem);
            true
        }
    });
    for (library, _, linked) in candidates {
        let symbol = symbol.name.to_vec();
        match linked {
            Some(linked) if linked.dropped_as_needed => {
                hints.push(Hint::DroppedAsNeeded { symbol, library });
            }
            // Linked and used: nothing to suggest.
            Some(_) => {}
            None => {
                let archive = (library.kind == LibraryKind::Shared)
                    .then(|| library::library_name(&library.path, ".so"))
                    .flatten()
                    .and_then(|name| index.linked_static(&name));
                match archive {
                    Some(archive) => hints.push(Hint::StaticOnly {
                        symbol,
                        library,
                        linked: archive.to_path_buf(),
                    }),
                    None => hints.push(Hint::MissingLibrary { symbol, library }),
                }
            }
        }
    }
    hints.truncate(MAX_LIBRARIES);

    if seen_objects.is_empty() && !definitions.is_empty() {
        let mut available: Vec<AvailableVersion> = Vec::new();
        for definition in definitions {
            let Some(library) = library_match(scope, index, definition) else {
                continue;
            };
            let version = AvailableVersion {
                library,
                default: !definition.hidden,
            };
            if !available.contains(&version) {
                available.push(version);
            }
        }
        // Archives have no versions; next to versioned definitions they are
        // noise.
        if available.iter().any(|v| v.library.version.is_some()) {
            available.retain(|v| v.library.version.is_some());
        }
        available.sort_by(|a, b| {
            a.library.path.cmp(&b.library.path).then_with(|| {
                natural_cmp(
                    a.library.version.as_deref().unwrap_or_default(),
                    b.library.version.as_deref().unwrap_or_default(),
                )
            })
        });
        if !available.is_empty() {
            hints.push(Hint::VersionMismatch {
                symbol: symbol.name.to_vec(),
                wanted: symbol
                    .version
                    .map(|v| String::from_utf8_lossy(v).into_owned()),
                available,
            });
        }
    }
    hints
}

/// The library name a file stands for: `m` for `libm.so`, `libm.a` and
/// `libm.so.6`; the file name up to `.so`/`.a` otherwise.
fn library_stem(path: &Path) -> String {
    let file = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = file.strip_prefix("lib").unwrap_or(&file);
    let end = [name.find(".so"), name.strip_suffix(".a").map(str::len)]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(name.len());
    name.get(..end).unwrap_or(name).to_string()
}

/// Compares version names with digit runs as numbers, so that `GLIBC_2.2.5`
/// sorts before `GLIBC_2.14`.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn runs(text: &str) -> Vec<(bool, &str)> {
        let mut out = Vec::new();
        let mut start = 0;
        let bytes = text.as_bytes();
        for i in 1..=bytes.len() {
            let boundary = i == bytes.len()
                || bytes.get(i).map(u8::is_ascii_digit)
                    != bytes.get(i.saturating_sub(1)).map(u8::is_ascii_digit);
            if boundary {
                let run = text.get(start..i).unwrap_or_default();
                out.push((run.as_bytes().first().is_some_and(u8::is_ascii_digit), run));
                start = i;
            }
        }
        out
    }
    let (ra, rb) = (runs(a), runs(b));
    for ((digits_a, x), (digits_b, y)) in ra.iter().zip(&rb) {
        let ordering = if *digits_a && *digits_b {
            let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
            x.len().cmp(&y.len()).then_with(|| x.cmp(y))
        } else {
            x.cmp(y)
        };
        if ordering.is_ne() {
            return ordering;
        }
    }
    ra.len().cmp(&rb.len())
}

/// A symbol name for display: demangled if `demangle` is set, otherwise as
/// is (converted to UTF-8 lossily).
#[must_use]
pub fn display_symbol(name: &[u8], demangle: bool) -> Cow<'_, str> {
    if demangle {
        crate::demangle::demangle(name)
    } else {
        String::from_utf8_lossy(name)
    }
}

/// Renders one hint as a note, the way lld words them.
#[must_use]
pub fn render(hint: &Hint, demangle: bool) -> String {
    let show = |name: &[u8]| display_symbol(name, demangle).into_owned();
    let place = |library: &LibraryMatch| {
        let path = library.path.display().to_string();
        if library.object == path
            || Path::new(&path)
                .file_name()
                .is_some_and(|n| n.to_string_lossy() == library.object)
        {
            path
        } else {
            format!("{} ({path})", library.object)
        }
    };
    match hint {
        Hint::MissingLibrary { symbol, library } => format!(
            "'{}' is defined in {}; did you forget {}?",
            show(symbol),
            place(library),
            library.flag
        ),
        Hint::DroppedAsNeeded { symbol, library } => format!(
            "'{}' is defined in {}, which --as-needed dropped because nothing needed it yet; put {} after the files that use it",
            show(symbol),
            place(library),
            library.flag
        ),
        Hint::StaticOnly {
            symbol,
            library,
            linked,
        } => format!(
            "'{}' is defined in {}, but -Bstatic linked {} instead, which does not define it",
            show(symbol),
            place(library),
            linked.display()
        ),
        Hint::VersionMismatch {
            symbol,
            wanted,
            available,
        } => {
            let mut by_library: Vec<(String, Vec<String>)> = Vec::new();
            for version in available {
                let name = match &version.library.version {
                    Some(v) if version.default => format!("{v} (default)"),
                    Some(v) => v.clone(),
                    None => "unversioned".to_string(),
                };
                let key = place(&version.library);
                match by_library.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, names)) => names.push(name),
                    None => by_library.push((key, vec![name])),
                }
            }
            let list = by_library
                .iter()
                .map(|(library, versions)| format!("{} in {library}", versions.join(", ")))
                .collect::<Vec<_>>()
                .join("; ");
            match wanted {
                Some(wanted) => format!(
                    "'{}' is not defined at version {wanted}; available: {list}",
                    show(symbol)
                ),
                None => format!(
                    "'{}' has no default version to bind to; available: {list}",
                    show(symbol)
                ),
            }
        }
        Hint::NearMiss { symbol, near } => {
            let candidate = show(&near.candidate);
            match near.kind {
                NearMissKind::CppDefinition => {
                    format!("did you mean to declare {candidate} as extern \"C\"?")
                }
                NearMissKind::CDefinition => format!(
                    "did you mean: extern \"C\" {candidate}? ('{}' has C++ linkage)",
                    show(symbol)
                ),
                NearMissKind::Underscore => {
                    format!("did you mean: {candidate}? (leading underscore)")
                }
                NearMissKind::Parameters => {
                    format!("did you mean: {candidate}? (parameter types differ)")
                }
                NearMissKind::Qualifiers => {
                    format!("did you mean: {candidate}? (qualifiers differ)")
                }
                NearMissKind::Scope => format!("did you mean: {candidate}? (different scope)"),
                NearMissKind::Spelling(_) => format!("did you mean: {candidate}?"),
            }
        }
    }
}

/// Adds `hints` to `diagnostic` as notes, rendered with [`render`].
#[must_use]
pub fn attach(mut diagnostic: Diagnostic, hints: &[Hint], demangle: bool) -> Diagnostic {
    for hint in hints {
        diagnostic = diagnostic.note(render(hint, demangle));
    }
    diagnostic
}

#[cfg(test)]
mod tests;
