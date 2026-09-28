//! Mapping symbols for the code qld generates itself.
//!
//! The Arm ABI marks every run of A32 code with `$a`, of Thumb code with
//! `$t` and of data inside a code section with `$d`; a disassembler that
//! finds none decodes in whatever state the previous run left. Input
//! mapping symbols are ordinary locals and are kept as they are, but the
//! PLT and the thunk pools are qld's own code, so qld marks them too, as
//! GNU ld and lld do.
//!
//! The three names live in one nine-byte block at the end of `.strtab`
//! ([`NAMES`]) that every mapping symbol points into. [`symbols`] is
//! called twice for a link: once while layout is sizing the trailers,
//! only to count the symbols, and once with the addresses layout assigned,
//! to write them. Both calls see the same sections and the same thunks,
//! so the counts agree.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::arm::THUNK_SIZE;
use crate::elf::arch::thunk::Thunks;

use super::PLT_HEADER_SIZE;

/// The names every mapping symbol qld writes points into, in one block.
pub const NAMES: &[u8] = b"$a\0$t\0$d\0";

/// Offset of `$a` in [`NAMES`].
pub const A: u32 = 0;
/// Offset of `$t` in [`NAMES`].
pub const T: u32 = 3;
/// Offset of `$d` in [`NAMES`].
pub const D: u32 = 6;

/// One mapping symbol: a `STT_NOTYPE` local of size 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mapping {
    /// Header index of the section it marks.
    pub shndx: u16,
    /// The address it marks (never with the Thumb bit set).
    pub address: u64,
    /// Offset of its name in [`NAMES`].
    pub name: u32,
}

/// The marks of a `.plt` that starts at offset 0, in a `dynamic` output.
///
/// The lazy header is four A32 instructions and the word they load the
/// `.got.plt` offset from; the entries after it are A32 throughout. A
/// static executable's `.plt` holds only IFUNC stubs, which are A32 too,
/// so one `$a` covers it.
#[must_use]
pub fn plt(dynamic: bool) -> &'static [(u64, u32)] {
    if dynamic {
        &[(0, A), (16, D), (PLT_HEADER_SIZE, A)]
    } else {
        &[(0, A)]
    }
}

/// The marks of the thunk of key `key` at offset 0: its instruction set,
/// and `$d` for the zeros after its instructions, since every thunk
/// occupies [`THUNK_SIZE`] bytes and only the position-independent A32
/// form fills them.
pub fn thunk(key: u64) -> impl Iterator<Item = (u64, u32)> {
    let thumb = key & (1 << 32) != 0;
    let pic = key & (1 << 33) != 0;
    // `movw`, `movt`, an `add` in the position-independent forms, and the
    // `bx`, which is two bytes in Thumb.
    let used = match (thumb, pic) {
        (false, false) => 12,
        (false, true) => 16,
        (true, false) => 10,
        (true, true) => 12,
    };
    let start = Some((0, if thumb { T } else { A }));
    let padding = (used < THUNK_SIZE).then_some((used, D));
    [start, padding].into_iter().flatten()
}

/// The mapping symbols of a link, sorted.
///
/// `plt_at` is the address and section header index of `.plt` when the
/// link has one, and `pool` gives them for the output section holding a
/// thunk (`None` when that section was dropped). Layout passes zero
/// addresses while it is only counting.
#[must_use]
pub fn symbols(
    plt_at: Option<(u64, u16)>,
    dynamic: bool,
    thunks: &Thunks,
    pool: &dyn Fn(u32) -> Option<(u64, u16)>,
) -> Vec<Mapping> {
    let mut out = Vec::new();
    if let Some((address, shndx)) = plt_at {
        for &(offset, name) in plt(dynamic) {
            out.push(Mapping {
                shndx,
                address: address.saturating_add(offset),
                name,
            });
        }
    }
    for entry in &thunks.entries {
        let Some((base, shndx)) = pool(entry.output) else {
            continue;
        };
        let start = base.saturating_add(entry.offset);
        for (offset, name) in thunk(entry.target) {
            out.push(Mapping {
                shndx,
                address: start.saturating_add(offset),
                name,
            });
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::arch::Arch;
    use crate::elf::arch::arm::thunk_key;

    fn thunks(keys: &[u64]) -> Thunks {
        let needed = keys.iter().map(|&key| (0u32, 0u32, key)).collect();
        Thunks::build_for(Arch::Arm, needed, &|_, _| 0x100)
    }

    #[test]
    fn names_hold_the_three_marks() {
        assert_eq!(NAMES.len(), 9);
        assert_eq!(NAMES.get(A as usize..A as usize + 2), Some(&b"$a"[..]));
        assert_eq!(NAMES.get(T as usize..T as usize + 2), Some(&b"$t"[..]));
        assert_eq!(NAMES.get(D as usize..D as usize + 2), Some(&b"$d"[..]));
    }

    #[test]
    fn a_thunk_is_marked_with_its_state_and_its_padding() {
        let marks: Vec<_> = thunk(thunk_key(0x2_0000, false, false)).collect();
        assert_eq!(marks, [(0, A), (12, D)]);
        let marks: Vec<_> = thunk(thunk_key(0x2_0000, false, true)).collect();
        assert_eq!(marks, [(0, A)]);
        let marks: Vec<_> = thunk(thunk_key(0x2_0001, true, false)).collect();
        assert_eq!(marks, [(0, T), (10, D)]);
        let marks: Vec<_> = thunk(thunk_key(0x2_0001, true, true)).collect();
        assert_eq!(marks, [(0, T), (12, D)]);
    }

    #[test]
    fn the_plt_header_and_its_word_are_marked() {
        let table = thunks(&[]);
        let header = symbols(Some((0x1000, 2)), true, &table, &|_| None);
        assert_eq!(
            header,
            [
                Mapping {
                    shndx: 2,
                    address: 0x1000,
                    name: A
                },
                Mapping {
                    shndx: 2,
                    address: 0x1010,
                    name: D
                },
                Mapping {
                    shndx: 2,
                    address: 0x1014,
                    name: A
                },
            ]
        );
        let stubs = symbols(Some((0x1000, 2)), false, &table, &|_| None);
        assert_eq!(stubs.len(), 1);
    }

    #[test]
    fn counting_and_writing_agree() {
        let table = thunks(&[
            thunk_key(0x2_0000, false, false),
            thunk_key(0x2_0001, true, true),
        ]);
        let counted = symbols(Some((0, 0)), true, &table, &|_| Some((0, 0)));
        let written = symbols(Some((0x1000, 2)), true, &table, &|_| Some((0x8000, 1)));
        assert_eq!(counted.len(), written.len());
        assert_eq!(written.len(), 3 + 2 + 2);
        // Sorted by section, then address.
        assert!(written.windows(2).all(|w| w[0] < w[1]));
        // A dropped output section has no pool.
        assert_eq!(symbols(None, true, &table, &|_| None).len(), 0);
    }
}
