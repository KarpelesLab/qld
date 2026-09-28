//! Motorola 68000 (`EM_68K`, ELF32 big-endian) relocations.
//!
//! **Workstream W55.** The m68k backend exists for AmigaOS Hunk output
//! ([`crate::hunk`]), so it covers what a static, non-position-independent
//! link needs: the absolute and PC-relative data and displacement fields,
//! `-r`, and the GOT-relative forms an assembler may emit. Dynamic output
//! is rejected before layout ([`super::Arch::unsupported_output`]): the
//! m68k PLT is a lazily bound two-instruction stub per entry that qld does
//! not synthesize yet, and producing one silently would be wrong.
//!
//! Every field is a plain big-endian data field, so
//! [`super::write_value_as`] writes all of them; there are no instruction
//! encodings to rewrite and no relaxation. Displacements are measured from
//! the field itself (`S + A - P`), which is how GAS and vasm emit their
//! addends, so the shared `S + A - P` computation applies unchanged.
//!
//! Relocations are `SHT_RELA` on m68k, as in the psABI, so no addend is
//! read from the field being patched.

#![deny(clippy::arithmetic_side_effects)]

use super::{Class, ClassifyContext, ClassifyError, GotKind, Kind, TlsMode, Width};

/// Declares the relocation type constants and their name lookup.
macro_rules! m68k_relocations {
    ($($name:ident = $value:expr,)*) => {
        $(
            #[doc = concat!("Relocation type `", stringify!($name), "`.")]
            pub const $name: u32 = $value;
        )*

        /// Returns the name of an m68k relocation type `r_type`, as
        /// `readelf` prints it.
        #[must_use]
        pub fn reloc_name(r_type: u32) -> Option<&'static str> {
            match r_type {
                $($name => Some(stringify!($name)),)*
                _ => None,
            }
        }
    };
}

m68k_relocations! {
    R_68K_NONE = 0,
    R_68K_32 = 1,
    R_68K_16 = 2,
    R_68K_8 = 3,
    R_68K_PC32 = 4,
    R_68K_PC16 = 5,
    R_68K_PC8 = 6,
    R_68K_GOT32 = 7,
    R_68K_GOT16 = 8,
    R_68K_GOT8 = 9,
    R_68K_GOT32O = 10,
    R_68K_GOT16O = 11,
    R_68K_GOT8O = 12,
    R_68K_PLT32 = 13,
    R_68K_PLT16 = 14,
    R_68K_PLT8 = 15,
    R_68K_PLT32O = 16,
    R_68K_PLT16O = 17,
    R_68K_PLT8O = 18,
    R_68K_COPY = 19,
    R_68K_GLOB_DAT = 20,
    R_68K_JMP_SLOT = 21,
    R_68K_RELATIVE = 22,
    R_68K_GNU_VTINHERIT = 23,
    R_68K_GNU_VTENTRY = 24,
    R_68K_TLS_GD32 = 25,
    R_68K_TLS_GD16 = 26,
    R_68K_TLS_GD8 = 27,
    R_68K_TLS_LDM32 = 28,
    R_68K_TLS_LDM16 = 29,
    R_68K_TLS_LDM8 = 30,
    R_68K_TLS_LDO32 = 31,
    R_68K_TLS_LDO16 = 32,
    R_68K_TLS_LDO8 = 33,
    R_68K_TLS_IE32 = 34,
    R_68K_TLS_IE16 = 35,
    R_68K_TLS_IE8 = 36,
    R_68K_TLS_LE32 = 37,
    R_68K_TLS_LE16 = 38,
    R_68K_TLS_LE8 = 39,
    R_68K_TLS_DTPMOD32 = 40,
    R_68K_TLS_DTPREL32 = 41,
    R_68K_TLS_TPREL32 = 42,
}

/// Size of a `.plt` entry, and of the PLT header, in GNU ld's
/// `elf32-m68k.c`. qld never writes one (dynamic m68k output is rejected
/// by [`super::Arch::unsupported_output`]); the size is here so layout
/// answers consistently.
pub const PLT_ENTRY_SIZE: u64 = 20;

/// A classification with no GOT entry.
const fn class(kind: Kind, width: Width) -> Class {
    Class::new(kind, width)
}

/// A classification that reads GOT entry `slot`.
const fn got(kind: Kind, width: Width, slot: GotKind) -> Class {
    Class::new(kind, width).through(slot)
}

/// Classifies m68k relocation `r_type`.
///
/// # Errors
///
/// [`ClassifyError::Unsupported`] for a type that is unknown, or that only
/// a dynamic linker may see.
pub fn classify(r_type: u32, context: ClassifyContext) -> Result<Class, ClassifyError> {
    use Kind as K;
    use Width as W;
    Ok(match r_type {
        R_68K_NONE | R_68K_GNU_VTINHERIT | R_68K_GNU_VTENTRY => class(K::None, W::None),
        R_68K_32 => class(K::Abs, W::Any32),
        R_68K_16 => class(K::Abs, W::Any16),
        R_68K_8 => class(K::Abs, W::Any8),
        // `PLT*` reach the symbol directly without a PLT: static output.
        R_68K_PC32 | R_68K_PLT32 => class(K::Pc, W::Any32),
        R_68K_PC16 | R_68K_PLT16 => class(K::Pc, W::I16),
        R_68K_PC8 | R_68K_PLT8 => class(K::Pc, W::I8),
        // `G + A - P`: the GOT entry addressed from the field.
        R_68K_GOT32 => class(K::Got, W::Any32),
        R_68K_GOT16 => class(K::Got, W::I16),
        R_68K_GOT8 => class(K::Got, W::I8),
        // `G + A - GOT`: the entry's offset in the GOT, which `(d16,a5)`
        // addressing adds to the GOT pointer.
        R_68K_GOT32O | R_68K_PLT32O => class(K::GotSlotRel, W::Any32),
        R_68K_GOT16O | R_68K_PLT16O => class(K::GotSlotRel, W::I16),
        R_68K_GOT8O | R_68K_PLT8O => class(K::GotSlotRel, W::I8),
        R_68K_TLS_LE32 => class(K::TpOff, W::Any32),
        R_68K_TLS_LE16 => class(K::TpOff, W::I16),
        R_68K_TLS_LE8 => class(K::TpOff, W::I8),
        R_68K_TLS_LDO32 => class(K::DtpOff, W::Any32),
        R_68K_TLS_LDO16 => class(K::DtpOff, W::I16),
        R_68K_TLS_LDO8 => class(K::DtpOff, W::I8),
        R_68K_TLS_IE32 => match context.tls {
            TlsMode::LocalExec => class(K::IeToLe, W::None),
            _ => got(K::GotSlotRel, W::Any32, GotKind::TpOff),
        },
        _ => return Err(ClassifyError::Unsupported),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(reloc_name(R_68K_PC32), Some("R_68K_PC32"));
        assert_eq!(reloc_name(R_68K_TLS_TPREL32), Some("R_68K_TLS_TPREL32"));
        assert_eq!(reloc_name(9999), None);
    }

    #[test]
    fn widths() {
        let context = ClassifyContext::static_exec(false);
        assert_eq!(
            classify(R_68K_32, context),
            Ok(Class::new(Kind::Abs, Width::Any32))
        );
        assert_eq!(
            classify(R_68K_PC16, context),
            Ok(Class::new(Kind::Pc, Width::I16))
        );
        // A dynamic linker's own types are not valid in an object.
        assert_eq!(
            classify(R_68K_JMP_SLOT, context),
            Err(ClassifyError::Unsupported)
        );
    }
}
