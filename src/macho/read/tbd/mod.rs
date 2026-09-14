//! Text-based dylib stubs (`.tbd`).
//!
//! Apple SDKs ship `.tbd` files instead of dylibs. They describe a library's
//! install name, versions, targets, re-exports and exported symbols:
//!
//! - **v3** (`--- !tapi-tbd-v3`) and **v4** (`--- !tapi-tbd` with
//!   `tbd-version: 4`) are YAML, parsed by the hand-written [`yaml`] module.
//!   A file can hold several documents: the first describes the library,
//!   the others the libraries it inlines (usually its re-exports). The older
//!   v1 (untagged or `!tapi-tbd-v1`) and v2 (`!tapi-tbd-v2`) share v3's
//!   layout and are read too.
//! - **v5** is JSON (`"tapi_tbd_version": 5`), parsed by [`json`], with the
//!   inlined libraries under `"libraries"`.
//!
//! [`TextStub::parse`] turns any of them into the same model:
//! [`StubLibrary`] per library, with target-scoped lists
//! ([`Scoped`], [`SymbolSection`]). [`StubLibrary::exports_for`] then
//! selects what one target exports, expanding Objective-C class, EH type
//! and ivar names into their symbol names.

pub mod json;
pub mod value;
pub mod yaml;

use std::fmt;

use super::arch::Arch;
use super::bytes::Source;
use super::commands::PackedVersion;
use super::consts::{
    PLATFORM_BRIDGEOS, PLATFORM_DRIVERKIT, PLATFORM_IOS, PLATFORM_IOSSIMULATOR,
    PLATFORM_MACCATALYST, PLATFORM_MACOS, PLATFORM_TVOS, PLATFORM_TVOSSIMULATOR, PLATFORM_WATCHOS,
    PLATFORM_WATCHOSSIMULATOR, PLATFORM_XROS, PLATFORM_XROS_SIMULATOR,
};
use crate::error::Result;
use value::Node;

/// Platform names as `.tbd` targets spell them, with their `PLATFORM_*`
/// values. The first name of each platform is the canonical one.
const PLATFORMS: &[(&str, u32)] = &[
    ("macos", PLATFORM_MACOS),
    ("ios", PLATFORM_IOS),
    ("tvos", PLATFORM_TVOS),
    ("watchos", PLATFORM_WATCHOS),
    ("bridgeos", PLATFORM_BRIDGEOS),
    ("maccatalyst", PLATFORM_MACCATALYST),
    ("ios-simulator", PLATFORM_IOSSIMULATOR),
    ("tvos-simulator", PLATFORM_TVOSSIMULATOR),
    ("watchos-simulator", PLATFORM_WATCHOSSIMULATOR),
    ("driverkit", PLATFORM_DRIVERKIT),
    ("xros", PLATFORM_XROS),
    ("xros-simulator", PLATFORM_XROS_SIMULATOR),
    // Aliases.
    ("macosx", PLATFORM_MACOS),
    ("ios-macabi", PLATFORM_MACCATALYST),
    ("iosmac", PLATFORM_MACCATALYST),
    ("iossimulator", PLATFORM_IOSSIMULATOR),
    ("tvossimulator", PLATFORM_TVOSSIMULATOR),
    ("watchossimulator", PLATFORM_WATCHOSSIMULATOR),
    ("xrsimulator", PLATFORM_XROS_SIMULATOR),
    ("visionos", PLATFORM_XROS),
    ("visionos-simulator", PLATFORM_XROS_SIMULATOR),
];

/// The canonical `.tbd` name of a platform.
#[must_use]
pub fn platform_name(platform: u32) -> Option<&'static str> {
    PLATFORMS
        .iter()
        .find(|&&(_, p)| p == platform)
        .map(|&(name, _)| name)
}

/// Parses a platform name.
#[must_use]
pub fn platform_from_name(name: &str) -> Option<u32> {
    PLATFORMS.iter().find(|&&(n, _)| n == name).map(|&(_, p)| p)
}

/// An architecture and platform, such as `arm64-macos`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StubTarget {
    /// The architecture name as written (`arm64`, `arm64e`, `x86_64`, …).
    pub arch_name: String,
    /// `PLATFORM_*`.
    pub platform: u32,
}

impl StubTarget {
    /// Builds a target from an architecture name and a platform.
    #[must_use]
    pub fn new(arch_name: &str, platform: u32) -> Self {
        Self {
            arch_name: arch_name.to_owned(),
            platform,
        }
    }

    /// Parses `arch-platform`, such as `arm64-macos` or
    /// `x86_64-ios-simulator`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (arch, platform) = text.split_once('-')?;
        if arch.is_empty() {
            return None;
        }
        let platform = platform_from_name(platform).or_else(|| platform.parse().ok())?;
        Some(Self::new(arch, platform))
    }

    /// The architecture, when qld knows its name.
    #[must_use]
    pub fn arch(&self) -> Option<Arch> {
        Arch::from_name(&self.arch_name)
    }

    /// Whether a library built for `self` can satisfy a link for `wanted`:
    /// the same platform and the same CPU type, whatever the subtype. This
    /// is lld's rule (`isArchABICompatible`): current SDK stubs often list
    /// only `arm64e`, and `arm64` links use them; `x86_64h` stubs serve
    /// `x86_64` links the same way.
    #[must_use]
    pub fn is_compatible_with(&self, wanted: &StubTarget) -> bool {
        if self.platform != wanted.platform {
            return false;
        }
        if self.arch_name == wanted.arch_name {
            return true;
        }
        match (self.arch(), wanted.arch()) {
            (Some(have), Some(want)) => have.cpu_type == want.cpu_type,
            _ => false,
        }
    }
}

impl fmt::Display for StubTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match platform_name(self.platform) {
            Some(name) => write!(f, "{}-{name}", self.arch_name),
            None => write!(f, "{}-{}", self.arch_name, self.platform),
        }
    }
}

/// A value that applies to some targets. An empty target list means every
/// target of the library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scoped<T> {
    /// The targets, or empty for all.
    pub targets: Vec<StubTarget>,
    /// The value.
    pub value: T,
}

impl<T> Scoped<T> {
    /// Whether this applies to `target`.
    #[must_use]
    pub fn applies_to(&self, target: &StubTarget) -> bool {
        self.targets.is_empty() || self.targets.contains(target)
    }
}

/// A list of symbols for some targets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SymbolSection {
    /// The targets, or empty for all.
    pub targets: Vec<StubTarget>,
    /// Plain symbols (`symbols`, v5 `global`).
    pub symbols: Vec<String>,
    /// Weak symbols: weak definitions in exports, weak references in
    /// undefineds.
    pub weak_symbols: Vec<String>,
    /// Thread-local symbols.
    pub thread_local_symbols: Vec<String>,
    /// Objective-C class names (without the `_OBJC_CLASS_$_` prefix).
    pub objc_classes: Vec<String>,
    /// Objective-C classes with exception types.
    pub objc_eh_types: Vec<String>,
    /// Objective-C instance variables (`Class.ivar`).
    pub objc_ivars: Vec<String>,
}

impl SymbolSection {
    /// Whether this section applies to `target`.
    #[must_use]
    pub fn applies_to(&self, target: &StubTarget) -> bool {
        self.targets.is_empty() || self.targets.contains(target)
    }
}

/// Library attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StubFlags {
    /// `flat_namespace`.
    pub flat_namespace: bool,
    /// `not_app_extension_safe`.
    pub not_app_extension_safe: bool,
    /// `not_for_dyld_shared_cache`.
    pub not_for_dyld_shared_cache: bool,
    /// `installapi`.
    pub installapi: bool,
}

/// One library described by a `.tbd` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StubLibrary {
    /// Format version: 1 to 5.
    pub tbd_version: u32,
    /// Targets the library supports.
    pub targets: Vec<StubTarget>,
    /// Install name.
    pub install_name: String,
    /// Current version (1.0 when absent).
    pub current_version: PackedVersion,
    /// Compatibility version (1.0 when absent).
    pub compatibility_version: PackedVersion,
    /// Swift ABI version (0 when absent).
    pub swift_abi_version: u32,
    /// Attributes.
    pub flags: StubFlags,
    /// Umbrella frameworks this library belongs to.
    pub parent_umbrellas: Vec<Scoped<String>>,
    /// Clients allowed to link against this library.
    pub allowable_clients: Vec<Scoped<Vec<String>>>,
    /// Re-exported libraries (install names).
    pub reexported_libraries: Vec<Scoped<Vec<String>>>,
    /// Run paths (v5 only).
    pub rpaths: Vec<Scoped<Vec<String>>>,
    /// Exported symbols.
    pub exports: Vec<SymbolSection>,
    /// Symbols re-exported from other libraries (still exported by this
    /// one).
    pub reexports: Vec<SymbolSection>,
    /// Undefined symbols (flat namespace libraries).
    pub undefineds: Vec<SymbolSection>,
}

/// How a stub symbol is exported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StubSymbolKind {
    /// A regular definition.
    Regular,
    /// A weak definition.
    Weak,
    /// A thread-local variable.
    ThreadLocal,
}

/// A symbol a library exports for one target.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StubSymbol {
    /// The symbol name, as it appears in object files (`_malloc`,
    /// `_OBJC_CLASS_$_NSObject`).
    pub name: String,
    /// How it is exported.
    pub kind: StubSymbolKind,
    /// Whether it came from a `reexports` section.
    pub reexported: bool,
}

impl StubLibrary {
    /// Whether the library lists exactly `target`.
    #[must_use]
    pub fn has_target(&self, target: &StubTarget) -> bool {
        self.targets.contains(target)
    }

    /// The target of this library to use for a link for `wanted`: `wanted`
    /// itself when listed, otherwise the first listed target that
    /// [`is_compatible_with`](StubTarget::is_compatible_with) it (in file
    /// order, so the choice is deterministic). `None` when the library
    /// cannot be used for `wanted`.
    #[must_use]
    pub fn select_target(&self, wanted: &StubTarget) -> Option<&StubTarget> {
        self.targets
            .iter()
            .find(|t| *t == wanted)
            .or_else(|| self.targets.iter().find(|t| t.is_compatible_with(wanted)))
    }

    /// The symbols a link for `wanted` gets from this library:
    /// [`exports_for`](Self::exports_for) the [selected](Self::select_target)
    /// target, or nothing when no listed target is compatible.
    #[must_use]
    pub fn exports_for_link(&self, wanted: &StubTarget) -> Vec<StubSymbol> {
        self.select_target(wanted)
            .map(|target| self.exports_for(target))
            .unwrap_or_default()
    }

    /// The symbols exported for `target`, sorted by name and kind, with
    /// duplicates removed. Objective-C classes expand to `_OBJC_CLASS_$_`
    /// and `_OBJC_METACLASS_$_` symbols, EH types to `_OBJC_EHTYPE_$_`, and
    /// ivars to `_OBJC_IVAR_$_`.
    #[must_use]
    pub fn exports_for(&self, target: &StubTarget) -> Vec<StubSymbol> {
        let mut out = Vec::new();
        let sections = self
            .exports
            .iter()
            .map(|s| (s, false))
            .chain(self.reexports.iter().map(|s| (s, true)));
        for (section, reexported) in sections {
            if !section.applies_to(target) {
                continue;
            }
            let mut push = |name: String, kind| {
                out.push(StubSymbol {
                    name,
                    kind,
                    reexported,
                });
            };
            for name in &section.symbols {
                push(name.clone(), StubSymbolKind::Regular);
            }
            for name in &section.weak_symbols {
                push(name.clone(), StubSymbolKind::Weak);
            }
            for name in &section.thread_local_symbols {
                push(name.clone(), StubSymbolKind::ThreadLocal);
            }
            for class in &section.objc_classes {
                push(format!("_OBJC_CLASS_$_{class}"), StubSymbolKind::Regular);
                push(
                    format!("_OBJC_METACLASS_$_{class}"),
                    StubSymbolKind::Regular,
                );
            }
            for class in &section.objc_eh_types {
                push(format!("_OBJC_EHTYPE_$_{class}"), StubSymbolKind::Regular);
            }
            for ivar in &section.objc_ivars {
                push(format!("_OBJC_IVAR_$_{ivar}"), StubSymbolKind::Regular);
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// The re-exported libraries for `target`.
    #[must_use]
    pub fn reexported_libraries_for(&self, target: &StubTarget) -> Vec<&str> {
        scoped_lists(&self.reexported_libraries, target)
    }

    /// The allowable clients for `target`.
    #[must_use]
    pub fn allowable_clients_for(&self, target: &StubTarget) -> Vec<&str> {
        scoped_lists(&self.allowable_clients, target)
    }

    /// The run paths for `target`.
    #[must_use]
    pub fn rpaths_for(&self, target: &StubTarget) -> Vec<&str> {
        scoped_lists(&self.rpaths, target)
    }

    /// The parent umbrella for `target`.
    #[must_use]
    pub fn parent_umbrella_for(&self, target: &StubTarget) -> Option<&str> {
        self.parent_umbrellas
            .iter()
            .find(|s| s.applies_to(target))
            .map(|s| s.value.as_str())
    }
}

fn scoped_lists<'s>(lists: &'s [Scoped<Vec<String>>], target: &StubTarget) -> Vec<&'s str> {
    let mut out = Vec::new();
    for list in lists.iter().filter(|l| l.applies_to(target)) {
        for item in &list.value {
            if !out.contains(&item.as_str()) {
                out.push(item.as_str());
            }
        }
    }
    out
}

/// A parsed `.tbd` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextStub {
    /// The libraries: the main one first, then the inlined ones.
    pub libraries: Vec<StubLibrary>,
}

impl TextStub {
    /// Parses a `.tbd` file of any supported version.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` with the byte offset of the problem for
    /// invalid UTF-8, YAML or JSON syntax errors, unknown versions, and
    /// missing or ill-typed required keys (targets, install name).
    pub fn parse(data: &[u8], source: Source<'_>) -> Result<Self> {
        let text = std::str::from_utf8(data).map_err(|e| {
            source.malformed(
                u64::try_from(e.valid_up_to()).unwrap_or(0),
                "text stub (invalid UTF-8)",
            )
        })?;
        let fail = |(offset, what): (usize, String)| {
            source.malformed(
                u64::try_from(offset).unwrap_or(u64::MAX),
                format!("text stub ({what})"),
            )
        };
        let trimmed = text.trim_start_matches('\u{feff}').trim_start();
        let libraries = if trimmed.starts_with('{') {
            let root = json::parse(text).map_err(fail)?;
            from_json(&root).map_err(fail)?
        } else {
            let documents = yaml::parse_stream(text).map_err(fail)?;
            if documents.is_empty() {
                return Err(fail((0, "no documents".to_owned())));
            }
            documents
                .iter()
                .map(from_yaml)
                .collect::<Result<Vec<_>, _>>()
                .map_err(fail)?
        };
        Ok(Self { libraries })
    }

    /// The main library.
    #[must_use]
    pub fn main(&self) -> Option<&StubLibrary> {
        self.libraries.first()
    }

    /// Finds a library (main or inlined) by install name.
    #[must_use]
    pub fn library(&self, install_name: &str) -> Option<&StubLibrary> {
        self.libraries
            .iter()
            .find(|l| l.install_name == install_name)
    }
}

type Extract<T> = Result<T, (usize, String)>;

fn bad<T>(node: &Node, what: impl Into<String>) -> Extract<T> {
    Err((node.offset, what.into()))
}

fn string_of(node: &Node, key: &str) -> Extract<String> {
    match node.as_str() {
        Some(s) => Ok(s.to_owned()),
        None => bad(node, format!("'{key}' is not a string")),
    }
}

fn strings_of(node: &Node, key: &str) -> Extract<Vec<String>> {
    match &node.value {
        value::Value::Scalar(s) => Ok(vec![s.clone()]),
        _ => node
            .as_seq()
            .ok_or((node.offset, format!("'{key}' is not a list")))?
            .iter()
            .map(|item| string_of(item, key))
            .collect(),
    }
}

fn optional_strings(map: &Node, key: &str) -> Extract<Vec<String>> {
    map.get(key).map_or(Ok(Vec::new()), |n| strings_of(n, key))
}

fn version_of(node: Option<&Node>, key: &str) -> Extract<PackedVersion> {
    let Some(node) = node else {
        return Ok(PackedVersion::new(1, 0, 0));
    };
    let text = string_of(node, key)?;
    PackedVersion::parse(&text).map_or_else(|| bad(node, format!("'{key}' is not a version")), Ok)
}

fn targets_of(node: &Node, key: &str) -> Extract<Vec<StubTarget>> {
    strings_of(node, key)?
        .iter()
        .map(|t| StubTarget::parse(t).ok_or((node.offset, format!("unknown target '{t}'"))))
        .collect()
}

fn flags_of(names: &[String], flags: &mut StubFlags) {
    for name in names {
        match name.as_str() {
            "flat_namespace" => flags.flat_namespace = true,
            "not_app_extension_safe" => flags.not_app_extension_safe = true,
            "not_for_dyld_shared_cache" => flags.not_for_dyld_shared_cache = true,
            "installapi" => flags.installapi = true,
            _ => {}
        }
    }
}

fn swift_abi_of(node: Option<&Node>) -> Extract<u32> {
    let Some(node) = node else { return Ok(0) };
    let text = string_of(node, "swift-abi-version")?;
    text.trim()
        .parse::<u32>()
        .or_else(|_| text.trim().parse::<f64>().map(|f| f as u32))
        .map_err(|_| {
            (
                node.offset,
                "'swift-abi-version' is not a number".to_owned(),
            )
        })
}

fn new_library(version: u32) -> StubLibrary {
    StubLibrary {
        tbd_version: version,
        targets: Vec::new(),
        install_name: String::new(),
        current_version: PackedVersion::new(1, 0, 0),
        compatibility_version: PackedVersion::new(1, 0, 0),
        swift_abi_version: 0,
        flags: StubFlags::default(),
        parent_umbrellas: Vec::new(),
        allowable_clients: Vec::new(),
        reexported_libraries: Vec::new(),
        rpaths: Vec::new(),
        exports: Vec::new(),
        reexports: Vec::new(),
        undefineds: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// v3 and v4 (YAML)
// ---------------------------------------------------------------------------

fn from_yaml(document: &yaml::Document) -> Extract<StubLibrary> {
    let root = &document.root;
    if root.as_map().is_none() {
        return bad(root, "document is not a mapping");
    }
    let offset = document.offset;
    match document.tag.as_deref() {
        Some("!tapi-tbd") => {
            let version = root
                .get("tbd-version")
                .and_then(Node::as_str)
                .ok_or((offset, "missing 'tbd-version'".to_owned()))?;
            if version.trim() != "4" {
                return Err((offset, format!("unsupported tbd-version {version}")));
            }
            from_v4(root)
        }
        Some("!tapi-tbd-v3") => from_v3(root, 3),
        Some("!tapi-tbd-v2") => from_v3(root, 2),
        // The first format had no tag.
        Some("!tapi-tbd-v1") => from_v3(root, 1),
        None if root.get("archs").is_some() => from_v3(root, 1),
        Some(tag) => Err((offset, format!("unknown document tag {tag}"))),
        None => Err((offset, "missing document tag".to_owned())),
    }
}

fn install_name(root: &Node) -> Extract<String> {
    let node = root
        .get("install-name")
        .ok_or((root.offset, "missing 'install-name'".to_owned()))?;
    string_of(node, "install-name")
}

fn from_v4(root: &Node) -> Extract<StubLibrary> {
    let mut lib = new_library(4);
    lib.targets = targets_of(
        root.get("targets")
            .ok_or((root.offset, "missing 'targets'".to_owned()))?,
        "targets",
    )?;
    lib.install_name = install_name(root)?;
    lib.current_version = version_of(root.get("current-version"), "current-version")?;
    lib.compatibility_version =
        version_of(root.get("compatibility-version"), "compatibility-version")?;
    lib.swift_abi_version = swift_abi_of(root.get("swift-abi-version"))?;
    flags_of(&optional_strings(root, "flags")?, &mut lib.flags);

    let sections = |key: &str| -> Extract<Vec<&Node>> {
        match root.get(key) {
            None => Ok(Vec::new()),
            Some(node) => node
                .as_seq()
                .map(|items| items.iter().collect())
                .ok_or((node.offset, format!("'{key}' is not a list"))),
        }
    };
    let scoped_targets = |item: &Node| -> Extract<Vec<StubTarget>> {
        item.get("targets")
            .map_or(Ok(Vec::new()), |t| targets_of(t, "targets"))
    };
    for item in sections("parent-umbrella")? {
        let umbrella = item
            .get("umbrella")
            .ok_or((item.offset, "missing 'umbrella'".to_owned()))?;
        lib.parent_umbrellas.push(Scoped {
            targets: scoped_targets(item)?,
            value: string_of(umbrella, "umbrella")?,
        });
    }
    for (key, list, dest) in [
        ("allowable-clients", "clients", &mut lib.allowable_clients),
        (
            "reexported-libraries",
            "libraries",
            &mut lib.reexported_libraries,
        ),
    ] {
        for item in sections(key)? {
            dest.push(Scoped {
                targets: scoped_targets(item)?,
                value: optional_strings(item, list)?,
            });
        }
    }
    for (key, dest, weak_key) in [
        ("exports", &mut lib.exports, "weak-symbols"),
        ("reexports", &mut lib.reexports, "weak-symbols"),
        ("undefineds", &mut lib.undefineds, "weak-symbols"),
    ] {
        for item in sections(key)? {
            if item.as_map().is_none() {
                return bad(item, format!("'{key}' entry is not a mapping"));
            }
            dest.push(SymbolSection {
                targets: scoped_targets(item)?,
                symbols: optional_strings(item, "symbols")?,
                weak_symbols: optional_strings(item, weak_key)?,
                thread_local_symbols: optional_strings(item, "thread-local-symbols")?,
                objc_classes: optional_strings(item, "objc-classes")?,
                objc_eh_types: optional_strings(item, "objc-eh-types")?,
                objc_ivars: optional_strings(item, "objc-ivars")?,
            });
        }
    }
    Ok(lib)
}

/// Maps a v3 platform to the platform of a target, which depends on the
/// architecture for simulators (Intel iOS slices run in the simulator).
fn v3_platform(platform: u32, arch: &str) -> u32 {
    let intel = matches!(arch, "i386" | "x86_64" | "x86_64h");
    match platform {
        PLATFORM_IOS if intel => PLATFORM_IOSSIMULATOR,
        PLATFORM_TVOS if intel => PLATFORM_TVOSSIMULATOR,
        PLATFORM_WATCHOS if intel => PLATFORM_WATCHOSSIMULATOR,
        other => other,
    }
}

/// Reads v1, v2 and v3 documents, which share one layout: architectures
/// and a platform at the top, target lists synthesized from them.
fn from_v3(root: &Node, version: u32) -> Extract<StubLibrary> {
    let mut lib = new_library(version);
    // Before v3, Objective-C class and ivar names carried the leading
    // underscore of their symbol names.
    let objc_names = |item: &Node, key: &str| -> Extract<Vec<String>> {
        let mut names = optional_strings(item, key)?;
        if version < 3 {
            for name in &mut names {
                if let Some(stripped) = name.strip_prefix('_') {
                    *name = stripped.to_owned();
                }
            }
        }
        Ok(names)
    };
    let archs_node = root
        .get("archs")
        .ok_or((root.offset, "missing 'archs'".to_owned()))?;
    let archs = strings_of(archs_node, "archs")?;
    let platform_node = root
        .get("platform")
        .ok_or((root.offset, "missing 'platform'".to_owned()))?;
    let platform_names = strings_of(platform_node, "platform")?;
    let mut platforms = Vec::new();
    for name in &platform_names {
        match name.as_str() {
            "zippered" => platforms.extend([PLATFORM_MACOS, PLATFORM_MACCATALYST]),
            other => platforms.push(
                platform_from_name(other)
                    .ok_or((platform_node.offset, format!("unknown platform '{other}'")))?,
            ),
        }
    }
    let synthesize = |archs: &[String]| -> Vec<StubTarget> {
        let mut targets = Vec::new();
        for &platform in &platforms {
            for arch in archs {
                if arch == "i386" && platform == PLATFORM_MACCATALYST {
                    continue;
                }
                let target = StubTarget::new(arch, v3_platform(platform, arch));
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        targets
    };
    lib.targets = synthesize(&archs);
    lib.install_name = install_name(root)?;
    lib.current_version = version_of(root.get("current-version"), "current-version")?;
    lib.compatibility_version =
        version_of(root.get("compatibility-version"), "compatibility-version")?;
    lib.swift_abi_version = swift_abi_of(root.get("swift-abi-version"))?;
    if version > 1 {
        flags_of(&optional_strings(root, "flags")?, &mut lib.flags);
    }
    if let Some(umbrella) = root.get("parent-umbrella") {
        lib.parent_umbrellas.push(Scoped {
            targets: Vec::new(),
            value: string_of(umbrella, "parent-umbrella")?,
        });
    }
    for (key, dest, weak_key) in [
        ("exports", &mut lib.exports, "weak-def-symbols"),
        ("undefineds", &mut lib.undefineds, "weak-ref-symbols"),
    ] {
        let Some(list) = root.get(key) else { continue };
        let items = list
            .as_seq()
            .ok_or((list.offset, format!("'{key}' is not a list")))?;
        for item in items {
            if item.as_map().is_none() {
                return bad(item, format!("'{key}' entry is not a mapping"));
            }
            let section_archs = optional_strings(item, "archs")?;
            let targets = synthesize(&section_archs);
            if key == "exports" {
                let mut clients = optional_strings(item, "allowable-clients")?;
                clients.extend(optional_strings(item, "allowed-clients")?);
                if !clients.is_empty() {
                    lib.allowable_clients.push(Scoped {
                        targets: targets.clone(),
                        value: clients,
                    });
                }
                let libraries = optional_strings(item, "re-exports")?;
                if !libraries.is_empty() {
                    lib.reexported_libraries.push(Scoped {
                        targets: targets.clone(),
                        value: libraries,
                    });
                }
            }
            dest.push(SymbolSection {
                // An explicit empty arch list applies to nothing; keep it
                // distinguishable from "all targets".
                targets: if targets.is_empty() {
                    vec![StubTarget::new("", 0)]
                } else {
                    targets
                },
                symbols: optional_strings(item, "symbols")?,
                weak_symbols: optional_strings(item, weak_key)?,
                thread_local_symbols: optional_strings(item, "thread-local-symbols")?,
                objc_classes: objc_names(item, "objc-classes")?,
                objc_eh_types: optional_strings(item, "objc-eh-types")?,
                objc_ivars: objc_names(item, "objc-ivars")?,
            });
        }
    }
    Ok(lib)
}

// ---------------------------------------------------------------------------
// v5 (JSON)
// ---------------------------------------------------------------------------

fn from_json(root: &Node) -> Extract<Vec<StubLibrary>> {
    let version = root
        .get("tapi_tbd_version")
        .ok_or((root.offset, "missing \"tapi_tbd_version\"".to_owned()))?;
    if version.as_str().map(str::trim) != Some("5") {
        return bad(version, "unsupported tapi_tbd_version (expected 5)");
    }
    let main = root
        .get("main_library")
        .ok_or((root.offset, "missing \"main_library\"".to_owned()))?;
    let mut libraries = vec![v5_library(main)?];
    if let Some(list) = root.get("libraries") {
        let items = list
            .as_seq()
            .ok_or((list.offset, "\"libraries\" is not an array".to_owned()))?;
        for item in items {
            libraries.push(v5_library(item)?);
        }
    }
    Ok(libraries)
}

/// The entries of the array at `key`, each required to be an object.
fn v5_entries<'n>(lib: &'n Node, key: &str) -> Extract<Vec<&'n Node>> {
    let Some(node) = lib.get(key) else {
        return Ok(Vec::new());
    };
    let items = node
        .as_seq()
        .ok_or((node.offset, format!("\"{key}\" is not an array")))?;
    for item in items {
        if item.as_map().is_none() {
            return bad(item, format!("\"{key}\" entry is not an object"));
        }
    }
    Ok(items.iter().collect())
}

fn v5_targets(entry: &Node) -> Extract<Vec<StubTarget>> {
    entry
        .get("targets")
        .map_or(Ok(Vec::new()), |t| targets_of(t, "targets"))
}

fn v5_library(node: &Node) -> Extract<StubLibrary> {
    if node.as_map().is_none() {
        return bad(node, "library is not an object");
    }
    let mut lib = new_library(5);
    for entry in v5_entries(node, "target_info")? {
        let target = entry
            .get("target")
            .ok_or((entry.offset, "missing \"target\"".to_owned()))?;
        lib.targets.extend(targets_of(target, "target")?);
    }
    if lib.targets.is_empty() {
        return bad(node, "missing \"target_info\"");
    }
    let first = |key: &str, field: &str| -> Extract<Option<&Node>> {
        Ok(v5_entries(node, key)?.first().and_then(|e| e.get(field)))
    };
    lib.install_name = match first("install_names", "name")? {
        Some(name) => string_of(name, "name")?,
        None => return bad(node, "missing \"install_names\""),
    };
    lib.current_version = version_of(first("current_versions", "version")?, "version")?;
    lib.compatibility_version = version_of(first("compatibility_versions", "version")?, "version")?;
    lib.swift_abi_version = swift_abi_of(first("swift_abi", "abi")?)?;
    for entry in v5_entries(node, "flags")? {
        flags_of(&optional_strings(entry, "attributes")?, &mut lib.flags);
    }
    for entry in v5_entries(node, "parent_umbrellas")? {
        let umbrella = entry
            .get("umbrella")
            .ok_or((entry.offset, "missing \"umbrella\"".to_owned()))?;
        lib.parent_umbrellas.push(Scoped {
            targets: v5_targets(entry)?,
            value: string_of(umbrella, "umbrella")?,
        });
    }
    for (key, list, dest) in [
        ("allowable_clients", "clients", &mut lib.allowable_clients),
        (
            "reexported_libraries",
            "names",
            &mut lib.reexported_libraries,
        ),
        ("rpaths", "paths", &mut lib.rpaths),
    ] {
        for entry in v5_entries(node, key)? {
            dest.push(Scoped {
                targets: v5_targets(entry)?,
                value: optional_strings(entry, list)?,
            });
        }
    }
    for (key, dest) in [
        ("exported_symbols", &mut lib.exports),
        ("reexported_symbols", &mut lib.reexports),
        ("undefined_symbols", &mut lib.undefineds),
    ] {
        for entry in v5_entries(node, key)? {
            let mut section = SymbolSection {
                targets: v5_targets(entry)?,
                ..SymbolSection::default()
            };
            for kind in ["data", "text"] {
                let Some(group) = entry.get(kind) else {
                    continue;
                };
                if group.as_map().is_none() {
                    return bad(group, format!("\"{kind}\" is not an object"));
                }
                section.symbols.extend(optional_strings(group, "global")?);
                section
                    .weak_symbols
                    .extend(optional_strings(group, "weak")?);
                section
                    .thread_local_symbols
                    .extend(optional_strings(group, "thread_local")?);
                section
                    .objc_classes
                    .extend(optional_strings(group, "objc_class")?);
                section
                    .objc_eh_types
                    .extend(optional_strings(group, "objc_eh_type")?);
                section
                    .objc_ivars
                    .extend(optional_strings(group, "objc_ivar")?);
            }
            dest.push(section);
        }
    }
    Ok(lib)
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn targets() {
        let t = StubTarget::parse("x86_64-ios-simulator").unwrap();
        assert_eq!(t.platform, PLATFORM_IOSSIMULATOR);
        assert_eq!(t.to_string(), "x86_64-ios-simulator");
        assert_eq!(
            StubTarget::parse("arm64e-macos").unwrap().arch(),
            Some(Arch::ARM64E)
        );
        assert!(StubTarget::parse("arm64").is_none());
        assert!(StubTarget::parse("arm64-plan9").is_none());
    }

    #[test]
    fn v5_scoped_lists_and_undefineds() {
        let text = r#"{
          "tapi_tbd_version": 5,
          "main_library": {
            "target_info": [ { "target": "arm64-macos", "min_deployment": "13" },
                             { "target": "arm64-ios" } ],
            "install_names": [ { "name": "@rpath/libq.dylib" } ],
            "current_versions": [ { "version": "3.2.1" } ],
            "swift_abi": [ { "abi": 7 } ],
            "flags": [ { "targets": [ "arm64-ios" ], "attributes": [ "flat_namespace" ] } ],
            "rpaths": [ { "targets": [ "arm64-macos" ], "paths": [ "@loader_path/../lib" ] } ],
            "parent_umbrellas": [ { "targets": [ "arm64-ios" ], "umbrella": "Kit" } ],
            "exported_symbols": [ { "text": { "global": [ "_f" ], "weak": [ "_w" ] },
                                    "data": { "thread_local": [ "_t" ] } } ],
            "undefined_symbols": [ { "targets": [ "arm64-macos" ],
                                     "data": { "global": [ "_u" ], "weak": [ "_uw" ] } } ]
          },
          "libraries": []
        }"#;
        let stub = TextStub::parse(text.as_bytes(), Source::new(Path::new("q.tbd"))).unwrap();
        let lib = stub.main().unwrap();
        let macos = StubTarget::parse("arm64-macos").unwrap();
        let ios = StubTarget::parse("arm64-ios").unwrap();
        assert_eq!(lib.current_version, PackedVersion::new(3, 2, 1));
        assert_eq!(lib.compatibility_version, PackedVersion::new(1, 0, 0));
        assert_eq!(lib.swift_abi_version, 7);
        assert!(lib.flags.flat_namespace);
        assert_eq!(lib.rpaths_for(&macos), ["@loader_path/../lib"]);
        assert!(lib.rpaths_for(&ios).is_empty());
        assert_eq!(lib.parent_umbrella_for(&ios), Some("Kit"));
        assert_eq!(lib.parent_umbrella_for(&macos), None);
        let kinds: Vec<_> = lib
            .exports_for(&ios)
            .into_iter()
            .map(|s| (s.name, s.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("_f".to_owned(), StubSymbolKind::Regular),
                ("_t".to_owned(), StubSymbolKind::ThreadLocal),
                ("_w".to_owned(), StubSymbolKind::Weak),
            ]
        );
        assert_eq!(lib.undefineds[0].weak_symbols, ["_uw"]);
        assert!(lib.undefineds[0].applies_to(&macos) && !lib.undefineds[0].applies_to(&ios));

        for bad in [
            r#"{"tapi_tbd_version": 4, "main_library": {}}"#,
            r#"{"tapi_tbd_version": 5}"#,
            r#"{"tapi_tbd_version": 5, "main_library": {"install_names": [{"name": "x"}]}}"#,
            r#"{"tapi_tbd_version": 5, "main_library": {"target_info": [{"target": "arm64-macos"}]}}"#,
            r#"{"tapi_tbd_version": 5, "main_library": {"target_info": [{"target": "bogus"}], "install_names": [{"name": "x"}]}}"#,
        ] {
            assert!(
                TextStub::parse(bad.as_bytes(), Source::new(Path::new("q.tbd"))).is_err(),
                "{bad}"
            );
        }
        for bad in [
            "--- !tapi-tbd-v9\narchs: [ x86_64 ]\nplatform: macosx\ninstall-name: /x\n",
            "--- !tapi-tbd-v3\narchs: [ x86_64 ]\nplatform: plan9\ninstall-name: /x\n",
            "--- !tapi-tbd-v3\nplatform: macosx\ninstall-name: /x\n",
            "--- !tapi-tbd\ntbd-version: 5\ntargets: [ arm64-macos ]\ninstall-name: /x\n",
            "--- !tapi-tbd\ntbd-version: 4\ninstall-name: /x\n",
            "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos ]\n",
            "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos ]\ninstall-name: /x\ncurrent-version: 1.2.3.4\n",
            "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos ]\ninstall-name: /x\nexports: [ a ]\n",
            "",
        ] {
            assert!(
                TextStub::parse(bad.as_bytes(), Source::new(Path::new("q.tbd"))).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn v1_and_v2() {
        let v2 = "--- !tapi-tbd-v2\narchs: [ armv7, arm64 ]\nplatform: ios\n\
                  flags: [ flat_namespace ]\ninstall-name: /u/l/libfoo.dylib\n\
                  exports:\n  - archs: [ arm64 ]\n    symbols: [ _sym ]\n    \
                  objc-classes: [ _NSFoo ]\n    objc-ivars: [ _NSFoo.bar ]\n...\n";
        let stub = TextStub::parse(v2.as_bytes(), Source::new(Path::new("v2.tbd"))).unwrap();
        let lib = stub.main().unwrap();
        assert_eq!(lib.tbd_version, 2);
        assert!(lib.flags.flat_namespace);
        let names: Vec<_> = lib
            .exports_for(&StubTarget::parse("arm64-ios").unwrap())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            [
                "_OBJC_CLASS_$_NSFoo",
                "_OBJC_IVAR_$_NSFoo.bar",
                "_OBJC_METACLASS_$_NSFoo",
                "_sym"
            ]
        );
        let v1 = "---\narchs: [ x86_64 ]\nplatform: macosx\nflags: [ flat_namespace ]\n\
                  install-name: /u/l/libbar.dylib\nexports:\n  - archs: [ x86_64 ]\n    \
                  allowed-clients: [ Client ]\n    symbols: [ _bar ]\n";
        let stub = TextStub::parse(v1.as_bytes(), Source::new(Path::new("v1.tbd"))).unwrap();
        let lib = stub.main().unwrap();
        assert_eq!(lib.tbd_version, 1);
        assert!(!lib.flags.flat_namespace);
        let macos = StubTarget::parse("x86_64-macos").unwrap();
        assert_eq!(lib.allowable_clients_for(&macos), ["Client"]);
    }

    #[test]
    fn v3_synthesizes_targets() {
        let text = "--- !tapi-tbd-v3\narchs: [ i386, x86_64 ]\nplatform: zippered\n\
                    install-name: /usr/lib/libz.1.dylib\ncurrent-version: 1.2.11\n\
                    exports:\n  - archs: [ x86_64 ]\n    symbols: [ _zlibVersion ]\n    \
                    re-exports: [ /usr/lib/libother.dylib ]\n...\n";
        let stub = TextStub::parse(text.as_bytes(), Source::new(Path::new("libz.tbd"))).unwrap();
        let lib = stub.main().unwrap();
        let names: Vec<_> = lib.targets.iter().map(ToString::to_string).collect();
        assert_eq!(names, ["i386-macos", "x86_64-macos", "x86_64-maccatalyst"]);
        let catalyst = StubTarget::parse("x86_64-maccatalyst").unwrap();
        assert_eq!(lib.exports_for(&catalyst).len(), 1);
        assert_eq!(
            lib.reexported_libraries_for(&catalyst),
            ["/usr/lib/libother.dylib"]
        );
        assert!(
            lib.exports_for(&StubTarget::parse("i386-macos").unwrap())
                .is_empty()
        );
    }

    #[test]
    fn arm64_links_use_arm64e_stubs() {
        // Current macOS SDKs list only arm64e for most of libSystem.
        let text = concat!(
            "--- !tapi-tbd\n",
            "tbd-version: 4\n",
            "targets: [ x86_64-macos, arm64e-macos ]\n",
            "install-name: /usr/lib/system/libsystem_c.dylib\n",
            "exports:\n",
            "  - targets: [ x86_64-macos, arm64e-macos ]\n",
            "    symbols: [ _malloc ]\n",
            "...\n",
        );
        let stub = TextStub::parse(text.as_bytes(), Source::new(Path::new("c.tbd"))).unwrap();
        let lib = stub.main().unwrap();
        let arm64 = StubTarget::parse("arm64-macos").unwrap();
        assert!(!lib.has_target(&arm64));
        assert_eq!(
            lib.select_target(&arm64)
                .map(ToString::to_string)
                .as_deref(),
            Some("arm64e-macos")
        );
        assert!(lib.exports_for(&arm64).is_empty());
        assert_eq!(lib.exports_for_link(&arm64).len(), 1);
        // Same CPU type is required, and the platform must match.
        assert!(
            lib.select_target(&StubTarget::parse("arm64_32-watchos").unwrap())
                .is_none()
        );
        assert!(
            lib.select_target(&StubTarget::parse("arm64-ios").unwrap())
                .is_none()
        );
        let x86_64h = StubTarget::parse("x86_64h-macos").unwrap();
        assert_eq!(
            lib.select_target(&x86_64h)
                .map(ToString::to_string)
                .as_deref(),
            Some("x86_64-macos")
        );
    }
}
