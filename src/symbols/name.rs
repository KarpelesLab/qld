//! Prehashed symbol names and input positions.
//!
//! A [`SymbolName`] is a name borrowed from an input mapping, plus an optional
//! version, plus a 64-bit hash computed once when the name is created. Format
//! readers build these inside their parallel parsing loops, so that the
//! symbol table never hashes a byte string again.

use core::cmp::Ordering;
use core::fmt;
use core::hash::{BuildHasher, Hasher};

use foldhash::fast::FixedState;

/// The per-hasher seed for symbol name hashes.
///
/// It is fixed, so a name hashes to the same value in every run on a given
/// kind of host. (foldhash reads bytes in native byte order and folds
/// differently on 32-bit hosts, so values differ between, say, x86-64 and
/// s390x.) Nothing observable depends on hash values: symbol IDs are assigned
/// by input order, see [`SymbolTable`](super::SymbolTable). A fixed seed keeps
/// table layout, and therefore performance, reproducible.
const NAME_HASH_SEED: u64 = 0x716c_645f_7379_6d73; // "qld_syms"

/// A symbol name borrowed from an input file, with its hash precomputed.
///
/// Names are raw bytes and are never assumed to be UTF-8. The version, when
/// present, is part of the name's identity: `foo` and `foo` at version
/// `GLIBC_2.2.5` are different symbols. How a format spells versions (for
/// ELF, `foo@V` versus `foo@@V`) and which spellings alias each other is the
/// format backend's business; it interns whichever keys it needs.
///
/// Building a `SymbolName` costs one hash of the bytes and does not allocate.
/// The value is `Copy`, 40 bytes on 64-bit hosts.
#[derive(Clone, Copy)]
pub struct SymbolName<'a> {
    bytes: &'a [u8],
    version: Option<&'a [u8]>,
    hash: u64,
}

impl<'a> SymbolName<'a> {
    /// Creates an unversioned name and hashes it.
    #[inline]
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self::with_version(bytes, None)
    }

    /// Creates a name with an optional version and hashes both.
    #[inline]
    #[must_use]
    pub fn with_version(bytes: &'a [u8], version: Option<&'a [u8]>) -> Self {
        let mut hasher = FixedState::with_seed(NAME_HASH_SEED).build_hasher();
        hasher.write(bytes);
        if let Some(version) = version {
            // foldhash mixes each write's length in, so ("ab", "c") and
            // ("a", "bc") already hash apart; the separator also keeps
            // `Some(b"")` apart from `None`.
            hasher.write_u8(b'@');
            hasher.write(version);
        }
        Self {
            bytes,
            version,
            hash: hasher.finish(),
        }
    }

    /// Returns the name bytes, without the version.
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Returns the version, if the name has one.
    #[inline]
    #[must_use]
    pub fn version(&self) -> Option<&'a [u8]> {
        self.version
    }

    /// Returns the precomputed 64-bit hash of the name and version.
    ///
    /// The value is stable across runs on the same kind of host (same
    /// endianness and pointer width) for a given qld version.
    #[inline]
    #[must_use]
    pub fn hash(&self) -> u64 {
        self.hash
    }

    /// Returns a value that renders the name for diagnostics: invalid UTF-8
    /// is replaced, and a version is appended as `name@version`.
    #[must_use]
    pub fn display(&self) -> impl fmt::Display + '_ {
        DisplayName(self)
    }

    /// Total order on name contents (bytes, then version), independent of the
    /// hash. Used to break ties deterministically.
    #[inline]
    pub(crate) fn cmp_contents(&self, other: &Self) -> Ordering {
        self.bytes
            .cmp(other.bytes)
            .then_with(|| self.version.cmp(&other.version))
    }
}

impl PartialEq for SymbolName<'_> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        // The hash comparison rejects almost every mismatch without touching
        // the (possibly cold) name bytes.
        self.hash == other.hash && self.bytes == other.bytes && self.version == other.version
    }
}

impl Eq for SymbolName<'_> {}

impl core::hash::Hash for SymbolName<'_> {
    /// Feeds only the precomputed hash, so hashing a `SymbolName` in a
    /// standard map is as cheap as hashing a `u64`.
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

impl fmt::Debug for SymbolName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SymbolName({:?})", self.display().to_string())
    }
}

struct DisplayName<'n, 'a>(&'n SymbolName<'a>);

impl fmt::Display for DisplayName<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(self.0.bytes))?;
        if let Some(version) = self.0.version {
            write!(f, "@{}", String::from_utf8_lossy(version))?;
        }
        Ok(())
    }
}

/// Where an input file sits in the link, as a sort key.
///
/// Every tie in symbol resolution (two weak definitions, two archive members
/// that can satisfy the same reference) is broken by this value: the lower
/// position wins. It orders by command-line input first, then by member
/// within an archive, so it is *not* the same as [`FileId`](crate::FileId)
/// order, which reflects when a file was loaded.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct InputPosition(u64);

impl InputPosition {
    /// Creates a position from the zero-based index of the input on the
    /// (flattened) command line and the zero-based index of the member within
    /// that input. Plain object files and shared libraries use member `0`.
    #[inline]
    #[must_use]
    pub const fn new(input: u32, member: u32) -> Self {
        Self(((input as u64) << 32) | member as u64)
    }

    /// Creates a position from a raw 64-bit sort key.
    #[inline]
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw 64-bit sort key.
    #[inline]
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Returns the command-line input index this position was built from.
    #[inline]
    #[must_use]
    pub const fn input(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// Returns the archive member index this position was built from.
    #[inline]
    #[must_use]
    pub const fn member(self) -> u32 {
        self.0 as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_deterministic_and_covers_version() {
        let a = SymbolName::new(b"printf");
        let b = SymbolName::new(b"printf");
        assert_eq!(a.hash(), b.hash());
        assert_eq!(a, b);

        let versioned = SymbolName::with_version(b"printf", Some(b"GLIBC_2.2.5"));
        assert_ne!(a, versioned);
        assert_ne!(a.hash(), versioned.hash());

        let split = SymbolName::with_version(b"printfGLIBC", Some(b"_2.2.5"));
        assert_ne!(versioned, split);
    }

    /// Guards against an accidental seed or algorithm change, which would
    /// silently change table layout between builds. foldhash reads bytes in
    /// native order, so the pinned values are for 64-bit little-endian hosts.
    #[test]
    #[cfg(all(target_endian = "little", target_pointer_width = "64"))]
    fn hash_value_is_pinned() {
        assert_eq!(SymbolName::new(b"main").hash(), 0x52ef_74d3_7756_ea78);
        assert_eq!(
            SymbolName::with_version(b"main", Some(b"V")).hash(),
            0x94f4_63c4_e7fe_fe93
        );
    }

    #[test]
    fn display_is_lossy_and_shows_version() {
        let name = SymbolName::with_version(b"f\xffo", Some(b"V1"));
        assert_eq!(name.display().to_string(), "f\u{fffd}o@V1");
    }

    #[test]
    fn positions_order_by_input_then_member() {
        let a = InputPosition::new(1, 900);
        let b = InputPosition::new(2, 0);
        assert!(a < b);
        assert_eq!(a.input(), 1);
        assert_eq!(a.member(), 900);
        assert_eq!(InputPosition::from_raw(a.raw()), a);
    }
}
