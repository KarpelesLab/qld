//! Per-symbol flag bits that parallel passes set without locks.

use core::fmt;
use core::ops::{BitAnd, BitOr, BitOrAssign, Not};
use core::sync::atomic::{AtomicU32, Ordering};

/// A set of per-symbol flag bits.
///
/// Bits 0–15 have format-neutral meanings, defined as associated constants.
/// Bits 16–31 are free for format backends: use [`SymbolFlags::backend`].
///
/// The symbol table stores one `AtomicU32` per symbol, so any thread can set
/// bits at any time (see [`SymbolTable::set_flags`](super::SymbolTable::set_flags)).
/// Setting a bit is idempotent and commutative, so the final flag state does
/// not depend on thread scheduling.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SymbolFlags(u32);

impl SymbolFlags {
    /// No bits set.
    pub const EMPTY: Self = Self(0);
    /// A live file has a non-weak reference to the symbol. Set by the
    /// resolution driver; drives archive extraction and undefined-symbol
    /// reports.
    pub const REFERENCED: Self = Self(1 << 0);
    /// A live file has a weak reference to the symbol. Set by the resolution
    /// driver. A weak reference alone neither extracts archive members nor
    /// makes an unresolved symbol an error.
    pub const WEAK_REFERENCED: Self = Self(1 << 1);
    /// The symbol's address escapes (it is used as a value, not only called).
    pub const ADDRESS_TAKEN: Self = Self(1 << 2);
    /// The symbol is exported from the output (dynamic symbol table, export
    /// table, export trie).
    pub const EXPORTED: Self = Self(1 << 3);
    /// A relocation needs a GOT entry holding the symbol's address.
    pub const NEEDS_GOT: Self = Self(1 << 4);
    /// A call needs a PLT (or stub) entry.
    pub const NEEDS_PLT: Self = Self(1 << 5);
    /// A non-PIC reference to a shared-library data symbol needs a copy
    /// relocation.
    pub const NEEDS_COPY_RELOC: Self = Self(1 << 6);
    /// A general-dynamic TLS access needs a module/offset GOT pair.
    pub const NEEDS_TLSGD: Self = Self(1 << 7);
    /// A TLS descriptor access needs a descriptor GOT entry.
    pub const NEEDS_TLSDESC: Self = Self(1 << 8);
    /// An initial-exec TLS access needs a GOT entry holding the TP offset.
    pub const NEEDS_GOTTPOFF: Self = Self(1 << 9);
    /// The symbol needs an entry in the dynamic symbol table even though it
    /// is not exported (for example, it is imported).
    pub const NEEDS_DYNSYM: Self = Self(1 << 10);
    /// The symbol's address must be canonical across the process, so a PLT
    /// entry doubles as its address.
    pub const NEEDS_CANONICAL_PLT: Self = Self(1 << 11);

    /// Index of the first bit available to format backends.
    pub const FIRST_BACKEND_BIT: u32 = 16;

    /// Returns the flag for backend-specific bit `n` (0–15), which is bit
    /// `16 + n` of the underlying word.
    ///
    /// # Panics
    ///
    /// Panics if `n` is 16 or more. In a `const` context this is a compile
    /// error.
    #[inline]
    #[must_use]
    pub const fn backend(n: u32) -> Self {
        assert!(n < 16, "backend flag index out of range");
        Self(1 << (Self::FIRST_BACKEND_BIT + n))
    }

    /// Creates a set from raw bits.
    #[inline]
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Returns the raw bits.
    #[inline]
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns `true` if every bit of `other` is set in `self`.
    #[inline]
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns `true` if any bit of `other` is set in `self`.
    #[inline]
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Returns `true` if no bit is set.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for SymbolFlags {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for SymbolFlags {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for SymbolFlags {
    type Output = Self;
    #[inline]
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl Not for SymbolFlags {
    type Output = Self;
    #[inline]
    fn not(self) -> Self {
        Self(!self.0)
    }
}

impl fmt::Debug for SymbolFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [&str; 12] = [
            "REFERENCED",
            "WEAK_REFERENCED",
            "ADDRESS_TAKEN",
            "EXPORTED",
            "NEEDS_GOT",
            "NEEDS_PLT",
            "NEEDS_COPY_RELOC",
            "NEEDS_TLSGD",
            "NEEDS_TLSDESC",
            "NEEDS_GOTTPOFF",
            "NEEDS_DYNSYM",
            "NEEDS_CANONICAL_PLT",
        ];
        f.write_str("SymbolFlags(")?;
        let mut first = true;
        for bit in 0..32u32 {
            if self.0 & (1 << bit) == 0 {
                continue;
            }
            if !first {
                f.write_str(" | ")?;
            }
            first = false;
            match NAMES.get(bit as usize) {
                Some(name) => f.write_str(name)?,
                None if bit >= Self::FIRST_BACKEND_BIT => {
                    write!(f, "BACKEND_{}", bit - Self::FIRST_BACKEND_BIT)?;
                }
                None => write!(f, "BIT_{bit}")?,
            }
        }
        f.write_str(")")
    }
}

/// Sets `flags` in `cell` and returns the bits that were set before.
///
/// Skips the write when every bit is already set, so hot symbols referenced
/// from many threads do not bounce their cache line between cores.
#[inline]
pub(crate) fn set(cell: &AtomicU32, flags: SymbolFlags) -> SymbolFlags {
    let current = cell.load(Ordering::Relaxed);
    if current & flags.0 == flags.0 {
        return SymbolFlags(current);
    }
    SymbolFlags(cell.fetch_or(flags.0, Ordering::Relaxed))
}

/// Clears `flags` in `cell` and returns the bits that were set before.
#[inline]
pub(crate) fn clear(cell: &AtomicU32, flags: SymbolFlags) -> SymbolFlags {
    SymbolFlags(cell.fetch_and(!flags.0, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_reports_previous_bits() {
        let cell = AtomicU32::new(0);
        let before = set(&cell, SymbolFlags::NEEDS_GOT);
        assert!(before.is_empty());
        let before = set(&cell, SymbolFlags::NEEDS_GOT | SymbolFlags::NEEDS_PLT);
        assert_eq!(before, SymbolFlags::NEEDS_GOT);
        let before = set(&cell, SymbolFlags::NEEDS_PLT);
        assert!(before.contains(SymbolFlags::NEEDS_GOT | SymbolFlags::NEEDS_PLT));
        let before = clear(&cell, SymbolFlags::NEEDS_GOT);
        assert!(before.contains(SymbolFlags::NEEDS_GOT));
        assert_eq!(
            SymbolFlags::from_bits(cell.load(Ordering::Relaxed)),
            SymbolFlags::NEEDS_PLT
        );
    }

    #[test]
    fn backend_bits_do_not_overlap_generic_bits() {
        let generic = SymbolFlags::from_bits(0xffff);
        for n in 0..16 {
            assert!(!generic.intersects(SymbolFlags::backend(n)));
        }
        assert_eq!(
            format!("{:?}", SymbolFlags::EXPORTED | SymbolFlags::backend(2)),
            "SymbolFlags(EXPORTED | BACKEND_2)"
        );
    }

    #[test]
    fn concurrent_sets_are_all_kept() {
        let cell = AtomicU32::new(0);
        std::thread::scope(|scope| {
            for bit in 0..16 {
                let cell = &cell;
                scope.spawn(move || {
                    for _ in 0..1000 {
                        set(cell, SymbolFlags::from_bits(1 << bit));
                    }
                });
            }
        });
        assert_eq!(cell.load(Ordering::Relaxed), 0xffff);
    }
}
