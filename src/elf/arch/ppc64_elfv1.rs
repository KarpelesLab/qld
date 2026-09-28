//! PowerPC64 big-endian, the ELFv1 ABI (`elf64ppc`).
//!
//! The relocations, the TOC and the instruction encodings are those of
//! [`super::ppc64`], read and written big-endian ([`crate::arch::ppc64`] is
//! generic over the byte order). What ELFv1 adds is **function
//! descriptors**: a global function symbol does not name its code but a
//! three-doubleword record in `.opd` holding the entry point, the TOC
//! pointer and the environment pointer. So:
//!
//! - `e_entry` is the address of `_start`'s descriptor, which falls out of
//!   resolving the symbol; a call through a pointer loads the first two
//!   doublewords and branches;
//! - a `bl` to a function, on the other hand, must reach the code, so the
//!   linker follows the descriptor: [`descriptor`] finds the
//!   `R_PPC64_ADDR64` that fills the first doubleword and the caller
//!   resolves it. Old compilers emitted a second, "dot" symbol (`.main`)
//!   for the code and relocated calls against it; then the symbol is an
//!   ordinary one in `.text` and nothing has to be followed.
//! - `R_PPC64_TOC`, which fills a descriptor's second doubleword, is the
//!   value of `.TOC.` ([`super::Kind::GotBase`]).
//!
//! The ABI also keeps the caller's TOC pointer at 40(r1) rather than the
//! 24(r1) of ELFv2 ([`LD_R2_40_R1`]), and `e_flags` records ABI version 1.
//!
//! Relocation offsets differ too: a relocation whose field is 16 bits names
//! that halfword, which is the second half of a big-endian instruction
//! (GNU ld's `d_offset`, [`crate::arch::ppc64::d_offset`]).

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::read::consts::ppc64::*;
use crate::elf::read::{Big, ElfFormat, Relocation, Relocations};
use crate::elf::refs::{Def, Refs};

use super::{Class, ClassifyContext, ClassifyError, Kind, Width};

/// The ABI version the output's `e_flags` records: ELFv1.
pub const ABI_VERSION: u32 = 1;

/// The size of one `.opd` function descriptor: the entry point, the TOC
/// pointer and the environment pointer.
pub const OPD_ENTRY_SIZE: u64 = 24;

/// `ld r2, 40(r1)`: ELFv1 restores the TOC pointer from 40(r1), where
/// ELFv2 uses 24(r1).
pub const LD_R2_40_R1: u32 = 0xe841_0028;

/// The name of the section that holds function descriptors.
pub const OPD_SECTION: &[u8] = b".opd";

/// Classifies ELFv1 relocation `r_type` at `offset` in section `data`.
///
/// Everything but `R_PPC64_TOC` is classified as for ELFv2
/// ([`super::ppc64::classify`]), in big-endian instruction words.
///
/// # Errors
///
/// As [`super::ppc64::classify`].
pub fn classify(
    r_type: u32,
    data: &[u8],
    offset: u64,
    context: ClassifyContext,
) -> Result<Class, ClassifyError> {
    // The second doubleword of a descriptor: the TOC pointer, with no
    // symbol and a zero addend.
    if super::ppc64::base_type(r_type) == R_PPC64_TOC {
        return Ok(Class::new(Kind::GotBase, Width::W64));
    }
    super::ppc64::classify::<Big>(r_type, data, offset, context)
}

/// The `R_PPC64_ADDR64` that fills the first doubleword of the function
/// descriptor at `target + addend`, with the index of the file that defines
/// it, when `target` is defined in a `.opd` section. The caller resolves
/// that relocation to get the code a `bl` must reach; the call's addend is
/// consumed here, as GNU ld's `opd_entry_value` does, because it selects
/// the descriptor rather than offsetting the code.
///
/// Returns `None` for a symbol that is not a descriptor (a "dot" symbol in
/// `.text`, a data symbol, an undefined one), which the caller then uses
/// as it is.
#[must_use]
pub fn descriptor<F: ElfFormat>(
    refs: &Refs<'_, '_, F>,
    target: &crate::elf::refs::Target,
    addend: i64,
) -> Option<(usize, Relocation)> {
    let Def::Section {
        file,
        section,
        value,
    } = target.def
    else {
        return None;
    };
    let value = value.checked_add_signed(addend)?;
    let object = refs.files.get(file)?.object.as_ref()?;
    let opd = object.section(section)?;
    if opd.name != OPD_SECTION {
        return None;
    }
    let Relocations::Rela(relas) = object
        .elf
        .relocation_section(opd.relocs, &object.section(opd.relocs)?.header)
        .ok()??
        .relocations
    else {
        return None;
    };
    // `.rela.opd` holds two relocations per descriptor (`ADDR64` then
    // `TOC`), sorted by offset, so a binary search finds the entry point's.
    let (mut low, mut high) = (0usize, relas.len());
    while low < high {
        let middle = low.wrapping_add(high.wrapping_sub(low) / 2);
        let candidate = relas.get(middle)?;
        match candidate.offset.cmp(&value) {
            std::cmp::Ordering::Equal => {
                return (candidate.r_type == R_PPC64_ADDR64).then_some((file, candidate));
            }
            std::cmp::Ordering::Less => low = middle.checked_add(1)?,
            std::cmp::Ordering::Greater => high = middle,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::arch::{GotKind, TlsMode};

    fn exec() -> ClassifyContext {
        ClassifyContext::static_exec(true)
    }

    #[test]
    fn toc_is_the_toc_base() {
        let toc = classify(R_PPC64_TOC, &[], 0, exec()).unwrap();
        assert_eq!(toc.kind, Kind::GotBase);
        assert_eq!(toc.width, Width::W64);
        assert!(toc.uses_got_base());
    }

    #[test]
    fn the_rest_is_classified_as_for_elfv2() {
        let call = classify(R_PPC64_REL24, &[], 0, exec()).unwrap();
        assert_eq!(call.kind, Kind::Pc);
        assert_eq!(
            call.width,
            Width::Ppc(crate::arch::ppc64::Field::Rel24),
            "the branch field is the ELFv2 one"
        );
        let entry = classify(R_PPC64_ADDR64, &[], 0, exec()).unwrap();
        assert_eq!(entry.kind, Kind::Abs);
        assert_eq!(entry.width, Width::W64);
        let got = classify(R_PPC64_GOT16_LO_DS, &[], 0, exec()).unwrap();
        assert_eq!(got.slot, GotKind::Address);
        let tls = ClassifyContext {
            tls: TlsMode::LocalExec,
            ..exec()
        };
        assert_eq!(
            classify(R_PPC64_GOT_TPREL16_HA, &[], 0, tls).unwrap().kind,
            Kind::IeToLe
        );
    }

    /// A 16-bit field is the second halfword of a big-endian instruction,
    /// so a field that rewrites the whole instruction starts two bytes
    /// before the relocation's place.
    #[test]
    fn half_fields_name_the_halfword() {
        use crate::arch::ppc64::{Field, d_offset};
        assert_eq!(d_offset::<Big>(), 2);
        assert_eq!(d_offset::<crate::elf::read::Little>(), 0);
        assert!(Field::LoDsToc.rewrites_from_half());
        assert!(Field::HaToc.rewrites_from_half());
        assert!(!Field::Rel24.rewrites_from_half());
        assert!(!Field::Ha.rewrites_from_half());
    }
}
