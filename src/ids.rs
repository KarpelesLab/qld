//! Dense identifiers for linker entities.
//!
//! Files, sections and symbols are referred to by 32-bit indices rather than
//! by references. Per-entity state lives in vectors indexed by these IDs, so
//! it can be shared across threads without borrow-checker or reference-count
//! overhead. See `docs/architecture.md` ("Indices, not pointers").

macro_rules! define_id {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            /// Creates an ID from a zero-based index.
            ///
            /// # Panics
            ///
            /// Panics if `index` does not fit in a `u32`. A link with more
            /// than 4 billion of anything is out of scope.
            #[inline]
            #[must_use]
            pub fn new(index: usize) -> Self {
                Self(u32::try_from(index).expect(concat!(stringify!($name), " index overflow")))
            }

            /// Returns the zero-based index this ID stands for.
            #[inline]
            #[must_use]
            pub fn index(self) -> usize {
                self.0 as usize
            }

            /// Returns the raw 32-bit value.
            #[inline]
            #[must_use]
            pub fn as_u32(self) -> u32 {
                self.0
            }

            /// Creates an ID from a raw 32-bit value.
            #[inline]
            #[must_use]
            pub fn from_u32(raw: u32) -> Self {
                Self(raw)
            }
        }

        impl core::fmt::Debug for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

define_id! {
    /// Identifies one input file: an object, a shared library, an archive, or
    /// one member of an archive. Members get their own `FileId` when they are
    /// extracted.
    FileId
}

define_id! {
    /// Identifies one input section, unique across all input files.
    SectionId
}

define_id! {
    /// Identifies one entry in the global symbol table. Symbols with the same
    /// name (and version, where the format has versions) share an ID no matter
    /// which file they came from.
    SymbolId
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_index_and_raw() {
        let id = SymbolId::new(42);
        assert_eq!(id.index(), 42);
        assert_eq!(id.as_u32(), 42);
        assert_eq!(SymbolId::from_u32(42), id);
    }

    #[test]
    fn ids_are_word_sized() {
        assert_eq!(size_of::<FileId>(), 4);
        assert_eq!(size_of::<SectionId>(), 4);
        assert_eq!(size_of::<SymbolId>(), 4);
    }
}
