//! Library and input path resolution.
//!
//! Resolves `-lfoo`, `-l:file` and linker script names against the search
//! directories and the sysroot. All file-system access goes through the
//! [`FileSystem`] trait, so resolution can be tested without touching disk.
//!
//! Search order, for each directory in turn (the first match wins):
//!
//! | Naming | Dynamic allowed | Static only (`-Bstatic`) |
//! | --- | --- | --- |
//! | ELF (GNU ld, lld, mold) | `libfoo.so`, `libfoo.a` | `libfoo.a` |
//! | MinGW (lld's order) | `libfoo.dll.a`, `foo.dll.a`, `libfoo.a`, `foo.lib`, `libfoo.dll`, `foo.dll` | `libfoo.a`, `foo.lib` |
//! | Darwin (`-search_paths_first`) | `libfoo.tbd`, `libfoo.dylib`, `libfoo.so`, `libfoo.a` | `libfoo.a` |
//!
//! `-l:file` searches for `file` exactly, in every naming.
//!
//! A search directory, or any path resolved with [`apply_sysroot`], that
//! starts with `=` or `$SYSROOT` has that prefix replaced by the sysroot. When
//! no sysroot is set the prefix is simply removed, as GNU ld does.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::args::{InputKind, InputSpec};
use crate::error::{Error, Result};

use super::table::Source;

/// The file-system queries path resolution needs.
pub trait FileSystem: Send + Sync {
    /// Whether `path` names an existing file (following symlinks), as opposed
    /// to nothing or a directory.
    fn is_file(&self, path: &Path) -> bool;
}

/// The real file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealFileSystem;

impl FileSystem for RealFileSystem {
    fn is_file(&self, path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|metadata| !metadata.is_dir())
    }
}

/// How library names map to file names.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LibraryNaming {
    /// ELF: `libfoo.so`, `libfoo.a`.
    #[default]
    Elf,
    /// MinGW: import libraries, static libraries, then DLLs.
    MinGw,
    /// Darwin: `.tbd` stubs, dylibs, then static libraries.
    Darwin,
}

impl LibraryNaming {
    /// The candidate file names for `-l<name>`, in search order.
    #[must_use]
    pub fn candidates(self, name: &str, static_only: bool) -> Vec<String> {
        let templates: &[(&str, &str, bool)] = match self {
            Self::Elf => &[("lib", ".so", true), ("lib", ".a", false)],
            Self::MinGw => &[
                ("lib", ".dll.a", true),
                ("", ".dll.a", true),
                ("lib", ".a", false),
                ("", ".lib", false),
                ("lib", ".dll", true),
                ("", ".dll", true),
            ],
            Self::Darwin => &[
                ("lib", ".tbd", true),
                ("lib", ".dylib", true),
                ("lib", ".so", true),
                ("lib", ".a", false),
            ],
        };
        templates
            .iter()
            .filter(|(_, _, dynamic)| !(static_only && *dynamic))
            .map(|(prefix, suffix, _)| format!("{prefix}{name}{suffix}"))
            .collect()
    }
}

/// Everything library resolution depends on.
#[derive(Clone, Copy)]
pub struct SearchContext<'a> {
    /// Library search directories (`-L`, then the defaults), in order.
    pub search_paths: &'a [PathBuf],
    /// The sysroot (`--sysroot`), if any.
    pub sysroot: Option<&'a Path>,
    /// How library names map to file names.
    pub naming: LibraryNaming,
    /// The file system to query.
    pub fs: &'a dyn FileSystem,
}

impl std::fmt::Debug for SearchContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchContext")
            .field("search_paths", &self.search_paths)
            .field("sysroot", &self.sysroot)
            .field("naming", &self.naming)
            .finish_non_exhaustive()
    }
}

/// Replaces a leading `=` or `$SYSROOT` in `path` with `sysroot`.
///
/// Paths without either prefix are returned unchanged. With no sysroot, the
/// prefix is removed.
#[must_use]
pub fn apply_sysroot(path: &Path, sysroot: Option<&Path>) -> PathBuf {
    let Some(rest) = strip_sysroot_prefix(path) else {
        return path.to_path_buf();
    };
    match sysroot {
        Some(sysroot) => {
            let mut joined = sysroot.as_os_str().to_os_string();
            joined.push(rest);
            PathBuf::from(joined)
        }
        None => PathBuf::from(rest),
    }
}

/// Returns what follows the sysroot prefix, if `path` has one.
fn strip_sysroot_prefix(path: &Path) -> Option<OsString> {
    let os = path.as_os_str();
    if let Some(text) = os.to_str() {
        let rest = text
            .strip_prefix('=')
            .or_else(|| text.strip_prefix("$SYSROOT"))?;
        return Some(OsString::from(rest));
    }
    strip_prefix_bytes(os)
}

#[cfg(unix)]
fn strip_prefix_bytes(os: &std::ffi::OsStr) -> Option<OsString> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = os.as_bytes();
    let rest = bytes
        .strip_prefix(b"=")
        .or_else(|| bytes.strip_prefix(b"$SYSROOT"))?;
    Some(std::ffi::OsStr::from_bytes(rest).to_os_string())
}

#[cfg(not(unix))]
fn strip_prefix_bytes(os: &std::ffi::OsStr) -> Option<OsString> {
    let text = os.to_string_lossy();
    let rest = text
        .strip_prefix('=')
        .or_else(|| text.strip_prefix("$SYSROOT"))?;
    Some(OsString::from(rest))
}

impl SearchContext<'_> {
    /// The search directories with sysroot prefixes applied.
    fn directories(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.search_paths
            .iter()
            .map(|dir| apply_sysroot(dir, self.sysroot))
    }

    /// Finds `-l<name>`: each candidate file name in each directory.
    #[must_use]
    pub fn find_library(&self, name: &str, static_only: bool) -> Option<PathBuf> {
        let candidates = self.naming.candidates(name, static_only);
        self.directories().find_map(|dir| {
            candidates
                .iter()
                .map(|candidate| dir.join(candidate))
                .find(|path| self.fs.is_file(path))
        })
    }

    /// Finds `-l:<file>`: that exact file name in each directory.
    #[must_use]
    pub fn find_exact(&self, file: &str) -> Option<PathBuf> {
        self.directories()
            .map(|dir| dir.join(file))
            .find(|path| self.fs.is_file(path))
    }

    /// Finds a linker script named by `-T` or as an input: the path itself
    /// (after sysroot substitution), then, for a relative path, each search
    /// directory.
    #[must_use]
    pub fn find_script(&self, path: &Path) -> Option<PathBuf> {
        let direct = apply_sysroot(path, self.sysroot);
        if self.fs.is_file(&direct) {
            return Some(direct);
        }
        if path.is_absolute() || strip_sysroot_prefix(path).is_some() {
            return None;
        }
        self.directories()
            .map(|dir| dir.join(path))
            .find(|candidate| self.fs.is_file(candidate))
    }

    /// Resolves one input specification to something the file table can load.
    ///
    /// Plain file inputs are not checked for existence here; loading reports
    /// a missing file with the operating system's error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] with [`std::io::ErrorKind::NotFound`] and a
    /// GNU-style message (`cannot find -lfoo`) when a library or script is not
    /// found.
    pub fn resolve(&self, spec: &InputSpec) -> Result<Source> {
        let not_found = |what: String| Error::Io {
            path: None,
            source: std::io::Error::new(std::io::ErrorKind::NotFound, what),
        };
        match &spec.kind {
            InputKind::File(path) => Ok(Source::Path(path.clone())),
            InputKind::Library(name) => self
                .find_library(name, spec.attrs.static_only)
                .map(Source::Path)
                .ok_or_else(|| not_found(format!("cannot find -l{name}"))),
            InputKind::LibraryExact(file) => self
                .find_exact(file)
                .map(Source::Path)
                .ok_or_else(|| not_found(format!("cannot find -l:{file}"))),
            InputKind::Script(path) => self
                .find_script(path)
                .map(Source::Path)
                .ok_or_else(|| not_found(format!("cannot find script {}", path.display()))),
            InputKind::Bytes { name, data } => Ok(Source::Bytes {
                name: PathBuf::from(name),
                data: data.clone(),
            }),
            #[allow(unreachable_patterns)]
            _ => Err(Error::Unimplemented(
                "resolving this kind of input (roadmap M2)".into(),
            )),
        }
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // Test code builds fixtures, not parses input.
mod tests {
    use super::*;
    use crate::args::InputAttrs;
    use std::collections::BTreeSet;

    struct FakeFs(BTreeSet<PathBuf>);

    impl FakeFs {
        fn new(files: &[&str]) -> Self {
            Self(files.iter().map(PathBuf::from).collect())
        }
    }

    impl FileSystem for FakeFs {
        fn is_file(&self, path: &Path) -> bool {
            self.0.contains(path)
        }
    }

    fn context<'a>(
        fs: &'a FakeFs,
        paths: &'a [PathBuf],
        sysroot: Option<&'a Path>,
    ) -> SearchContext<'a> {
        SearchContext {
            search_paths: paths,
            sysroot,
            naming: LibraryNaming::Elf,
            fs,
        }
    }

    fn spec(kind: InputKind, static_only: bool) -> InputSpec {
        InputSpec {
            kind,
            attrs: InputAttrs {
                static_only,
                ..InputAttrs::default()
            },
            position: 0,
        }
    }

    #[test]
    fn shared_before_static_per_directory() {
        let fs = FakeFs::new(&["/a/libfoo.a", "/b/libfoo.so", "/b/libfoo.a", "/c/libbar.so"]);
        let paths = [
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        let ctx = context(&fs, &paths, None);
        // The first directory with any match wins, even if it only has `.a`.
        assert_eq!(ctx.find_library("foo", false), Some("/a/libfoo.a".into()));
        assert_eq!(ctx.find_library("bar", false), Some("/c/libbar.so".into()));
        assert_eq!(ctx.find_library("bar", true), None);
        assert_eq!(ctx.find_library("baz", false), None);

        let paths = [PathBuf::from("/b")];
        let ctx = context(&fs, &paths, None);
        assert_eq!(ctx.find_library("foo", false), Some("/b/libfoo.so".into()));
        assert_eq!(ctx.find_library("foo", true), Some("/b/libfoo.a".into()));
    }

    #[test]
    fn exact_names_and_resolve_errors() {
        let fs = FakeFs::new(&["/lib/crt1.o", "/lib/libm.so.6"]);
        let paths = [PathBuf::from("/usr/lib"), PathBuf::from("/lib")];
        let ctx = context(&fs, &paths, None);
        let source = ctx
            .resolve(&spec(InputKind::LibraryExact("libm.so.6".into()), true))
            .unwrap();
        assert_eq!(source, Source::Path("/lib/libm.so.6".into()));

        let error = ctx
            .resolve(&spec(InputKind::Library("nope".into()), false))
            .unwrap_err();
        assert_eq!(error.to_string(), "cannot find -lnope");
        let error = ctx
            .resolve(&spec(InputKind::LibraryExact("x.a".into()), false))
            .unwrap_err();
        assert_eq!(error.to_string(), "cannot find -l:x.a");

        let file = ctx
            .resolve(&spec(InputKind::File("rel/a.o".into()), false))
            .unwrap();
        assert_eq!(file, Source::Path("rel/a.o".into()));
    }

    #[test]
    fn sysroot_prefixes() {
        let root = Path::new("/sysroot");
        assert_eq!(
            apply_sysroot(Path::new("=/usr/lib"), Some(root)),
            PathBuf::from("/sysroot/usr/lib")
        );
        assert_eq!(
            apply_sysroot(Path::new("$SYSROOT/lib"), Some(root)),
            PathBuf::from("/sysroot/lib")
        );
        assert_eq!(
            apply_sysroot(Path::new("=/usr/lib"), None),
            PathBuf::from("/usr/lib")
        );
        assert_eq!(
            apply_sysroot(Path::new("/usr/lib"), Some(root)),
            PathBuf::from("/usr/lib")
        );

        let fs = FakeFs::new(&["/sysroot/usr/lib/libc.so", "/usr/lib/libz.a"]);
        let paths = [PathBuf::from("=/usr/lib"), PathBuf::from("/usr/lib")];
        let ctx = context(&fs, &paths, Some(root));
        assert_eq!(
            ctx.find_library("c", false),
            Some("/sysroot/usr/lib/libc.so".into())
        );
        assert_eq!(ctx.find_library("z", false), Some("/usr/lib/libz.a".into()));
    }

    #[test]
    fn scripts_search_directories_for_relative_names() {
        let fs = FakeFs::new(&["/ld/elf.x", "local.ld", "/sysroot/abs.ld"]);
        let paths = [PathBuf::from("/ld")];
        let root = Path::new("/sysroot");
        let ctx = context(&fs, &paths, Some(root));
        assert_eq!(
            ctx.find_script(Path::new("local.ld")),
            Some("local.ld".into())
        );
        assert_eq!(
            ctx.find_script(Path::new("elf.x")),
            Some("/ld/elf.x".into())
        );
        assert_eq!(ctx.find_script(Path::new("/elf.x")), None);
        assert_eq!(
            ctx.find_script(Path::new("=/abs.ld")),
            Some("/sysroot/abs.ld".into())
        );
        assert!(
            ctx.resolve(&spec(InputKind::Script("missing.ld".into()), false))
                .is_err()
        );
    }

    #[test]
    fn mingw_and_darwin_orders() {
        assert_eq!(
            LibraryNaming::MinGw.candidates("foo", false),
            [
                "libfoo.dll.a",
                "foo.dll.a",
                "libfoo.a",
                "foo.lib",
                "libfoo.dll",
                "foo.dll"
            ]
        );
        assert_eq!(
            LibraryNaming::MinGw.candidates("foo", true),
            ["libfoo.a", "foo.lib"]
        );
        assert_eq!(
            LibraryNaming::Darwin.candidates("z", false),
            ["libz.tbd", "libz.dylib", "libz.so", "libz.a"]
        );
        assert_eq!(LibraryNaming::Darwin.candidates("z", true), ["libz.a"]);
    }

    #[test]
    fn bytes_inputs_pass_through() {
        let fs = FakeFs::new(&[]);
        let ctx = context(&fs, &[], None);
        let data: std::sync::Arc<[u8]> = std::sync::Arc::from(&b"x"[..]);
        let source = ctx
            .resolve(&spec(
                InputKind::Bytes {
                    name: "mem".into(),
                    data: data.clone(),
                },
                false,
            ))
            .unwrap();
        assert_eq!(
            source,
            Source::Bytes {
                name: "mem".into(),
                data
            }
        );
    }
}
