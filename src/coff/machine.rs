//! What differs between the PE machines qld links: x86-64 (PE32+), i386
//! (PE32) and ARM64 (PE32+).
//!
//! | | x86-64 | i386 | ARM64 |
//! | --- | --- | --- | --- |
//! | Optional header | PE32+ (240 bytes) | PE32 (224 bytes) | PE32+ |
//! | Pointer and IAT entry | 8 bytes | 4 bytes | 8 bytes |
//! | C symbol decoration | none | leading `_`, stdcall `@N` | none |
//! | Absolute address base relocation | `DIR64` | `HIGHLOW` | `DIR64` |
//! | `RUNTIME_FUNCTION` in `.pdata` | 12 bytes | — | 8 bytes |
//! | Default image base (EXE / DLL) | `0x140000000` / `0x180000000` | `0x400000` / `0x10000000` | as x86-64 |
//!
//! GNU `ld` calls the three emulations `i386pep`, `i386pe` and `arm64pe`.
//! i386 is the only *underscoring* target: the C name `foo` is the COFF
//! symbol `_foo`, a `__stdcall` function taking eight bytes of arguments is
//! `_foo@8`, and a `__fastcall` one is `@foo@8`. Exports, import libraries,
//! `.def` files and the linker's own symbol names are all written in C
//! names and decorated with [`Machine::decorate`].

#![deny(clippy::arithmetic_side_effects)]

use crate::error::{Error, Result};

use super::read::consts::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_ARM64EC,
    IMAGE_FILE_MACHINE_ARM64X, IMAGE_FILE_MACHINE_I386,
};

/// A machine qld writes PE images for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Machine {
    /// x86-64 (`IMAGE_FILE_MACHINE_AMD64`), PE32+.
    #[default]
    Amd64,
    /// i386 (`IMAGE_FILE_MACHINE_I386`), PE32.
    I386,
    /// ARM64 (`IMAGE_FILE_MACHINE_ARM64`), PE32+.
    Arm64,
}

impl Machine {
    /// The machine a COFF `Machine` value names.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unimplemented`] for machines qld does not link:
    /// ARM64EC and ARM64X hybrids, 32-bit ARM and the rest.
    pub fn from_coff(machine: u16) -> Result<Self> {
        match machine {
            IMAGE_FILE_MACHINE_AMD64 => Ok(Self::Amd64),
            IMAGE_FILE_MACHINE_I386 => Ok(Self::I386),
            IMAGE_FILE_MACHINE_ARM64 => Ok(Self::Arm64),
            IMAGE_FILE_MACHINE_ARM64EC | IMAGE_FILE_MACHINE_ARM64X => Err(Error::Unimplemented(
                "ARM64EC and ARM64X images for PE/COFF (only plain ARM64 is supported)".into(),
            )),
            other => Err(Error::Unimplemented(format!(
                "PE output for machine {other:#06x} (x86-64, i386 and ARM64 are supported)"
            ))),
        }
    }

    /// The machine for a COFF `Machine` value, or x86-64 for one qld does
    /// not link (the check that rejects it happens elsewhere).
    #[must_use]
    pub fn or_default(machine: u16) -> Self {
        Self::from_coff(machine).unwrap_or(Self::Amd64)
    }

    /// The COFF `Machine` value.
    #[must_use]
    pub const fn coff(self) -> u16 {
        match self {
            Self::Amd64 => IMAGE_FILE_MACHINE_AMD64,
            Self::I386 => IMAGE_FILE_MACHINE_I386,
            Self::Arm64 => IMAGE_FILE_MACHINE_ARM64,
        }
    }

    /// Whether the image is PE32 rather than PE32+.
    #[must_use]
    pub const fn is_pe32(self) -> bool {
        matches!(self, Self::I386)
    }

    /// Size of a pointer, and of an import lookup or address table entry.
    #[must_use]
    pub const fn pointer_size(self) -> u32 {
        if self.is_pe32() { 4 } else { 8 }
    }

    /// Whether C names get a leading underscore (i386 only).
    #[must_use]
    pub const fn underscores(self) -> bool {
        self.is_pe32()
    }

    /// The COFF symbol for the C name `name`: `_name` on i386, unless the
    /// name is already a fastcall (`@name@N`) or a C++ (`?name@@...`)
    /// symbol, which carry no underscore.
    #[must_use]
    pub fn decorate(self, name: &[u8]) -> Vec<u8> {
        if self.underscores() && !matches!(name.first(), Some(b'@' | b'?')) {
            let mut out = Vec::with_capacity(name.len().saturating_add(1));
            out.push(b'_');
            out.extend_from_slice(name);
            out
        } else {
            name.to_vec()
        }
    }

    /// The C name of the COFF symbol `name`: the inverse of
    /// [`decorate`](Self::decorate), which drops the leading underscore on
    /// i386. A name without one (fastcall, C++) comes back unchanged.
    #[must_use]
    pub fn undecorate(self, name: &[u8]) -> &[u8] {
        if self.underscores() {
            name.strip_prefix(b"_").unwrap_or(name)
        } else {
            name
        }
    }

    /// Size of the optional header with its sixteen data directories.
    #[must_use]
    pub const fn optional_header_size(self) -> usize {
        if self.is_pe32() { 224 } else { 240 }
    }

    /// The default image base, as GNU `ld` chooses it without
    /// `--enable-auto-image-base`.
    #[must_use]
    pub const fn default_image_base(self, dll: bool) -> u64 {
        match (self.is_pe32(), dll) {
            (true, false) => 0x40_0000,
            (true, true) => 0x1000_0000,
            (false, false) => 0x1_4000_0000,
            (false, true) => 0x1_8000_0000,
        }
    }

    /// The `IMAGE_REL_BASED_*` type of a pointer-sized absolute address.
    #[must_use]
    pub const fn pointer_base_reloc(self) -> u16 {
        if self.is_pe32() {
            super::reloc::IMAGE_REL_BASED_HIGHLOW
        } else {
            super::reloc::IMAGE_REL_BASED_DIR64
        }
    }

    /// Size of one `.pdata` entry (`RUNTIME_FUNCTION`), or 0 when the
    /// machine has no table-based unwinding.
    #[must_use]
    pub const fn pdata_entry_size(self) -> usize {
        match self {
            Self::Amd64 => 12,
            Self::Arm64 => 8,
            Self::I386 => 0,
        }
    }

    /// Size of `IMAGE_TLS_DIRECTORY`: six pointer-or-`u32` fields.
    #[must_use]
    pub const fn tls_directory_size(self) -> u32 {
        if self.is_pe32() { 24 } else { 40 }
    }

    /// The entry point GNU `ld` looks for when none is given, by output
    /// kind, as a COFF symbol.
    #[must_use]
    pub fn default_entry(self, dll: bool, gui: bool) -> Vec<u8> {
        match (dll, gui) {
            // `DllMainCRTStartup` is `__stdcall` with three arguments.
            (true, _) if self.underscores() => b"_DllMainCRTStartup@12".to_vec(),
            (true, _) => b"DllMainCRTStartup".to_vec(),
            (false, true) => self.decorate(b"WinMainCRTStartup"),
            (false, false) => self.decorate(b"mainCRTStartup"),
        }
    }

    /// The BFD target names GNU `ld` accepts in `--oformat` for this
    /// machine's images.
    #[must_use]
    pub const fn bfd_names(self) -> &'static [&'static str] {
        match self {
            Self::Amd64 => &["pei-x86-64", "pe-x86-64"],
            Self::I386 => &["pei-i386", "pe-i386"],
            Self::Arm64 => &["pei-aarch64-little", "pe-aarch64-little"],
        }
    }
}

/// Splits a stdcall or fastcall name into its body and its `@N` suffix:
/// `_foo@8` gives `(_foo, Some(8))`, `@foo@8` gives `(@foo, Some(8))`, and
/// anything else (including C++ names, which start with `?`) gives
/// `(name, None)`.
#[must_use]
pub fn split_stdcall(name: &[u8]) -> (&[u8], Option<u32>) {
    if name.first() == Some(&b'?') {
        return (name, None);
    }
    let Some(at) = name.iter().rposition(|&byte| byte == b'@') else {
        return (name, None);
    };
    if at == 0 {
        return (name, None);
    }
    let digits = name.get(at.saturating_add(1)..).unwrap_or_default();
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return (name, None);
    }
    let value = std::str::from_utf8(digits)
        .ok()
        .and_then(|text| text.parse::<u32>().ok());
    match value {
        Some(value) => (name.get(..at).unwrap_or(name), Some(value)),
        None => (name, None),
    }
}

/// The name `--kill-at` exports for `name`: without the `@N` suffix, and
/// without the leading `@` of a fastcall name. Other names are unchanged.
#[must_use]
pub fn kill_at(name: &[u8]) -> &[u8] {
    match split_stdcall(name) {
        (body, Some(_)) => body.strip_prefix(b"@").unwrap_or(body),
        (_, None) => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoration_round_trips() {
        let i386 = Machine::I386;
        assert_eq!(i386.decorate(b"main"), b"_main");
        assert_eq!(i386.decorate(b"@fast@8"), b"@fast@8");
        assert_eq!(i386.decorate(b"?cxx@@YAXXZ"), b"?cxx@@YAXXZ");
        assert_eq!(i386.undecorate(b"_main"), b"main");
        assert_eq!(i386.undecorate(b"@fast@8"), b"@fast@8");
        assert_eq!(Machine::Amd64.decorate(b"main"), b"main");
        assert_eq!(Machine::Arm64.undecorate(b"_main"), b"_main");
    }

    #[test]
    fn stdcall_suffixes() {
        assert_eq!(split_stdcall(b"_foo@8"), (&b"_foo"[..], Some(8)));
        assert_eq!(split_stdcall(b"@foo@12"), (&b"@foo"[..], Some(12)));
        assert_eq!(split_stdcall(b"_foo"), (&b"_foo"[..], None));
        assert_eq!(split_stdcall(b"foo@bar"), (&b"foo@bar"[..], None));
        assert_eq!(split_stdcall(b"?f@@YAXXZ"), (&b"?f@@YAXXZ"[..], None));
        assert_eq!(kill_at(b"foo@8"), b"foo");
        assert_eq!(kill_at(b"@foo@8"), b"foo");
        assert_eq!(kill_at(b"plain"), b"plain");
    }

    #[test]
    fn machine_properties() {
        assert_eq!(
            Machine::from_coff(IMAGE_FILE_MACHINE_I386).unwrap(),
            Machine::I386
        );
        assert!(Machine::from_coff(IMAGE_FILE_MACHINE_ARM64EC).is_err());
        assert!(Machine::from_coff(0x1c4).is_err());
        assert_eq!(Machine::I386.default_image_base(false), 0x40_0000);
        assert_eq!(
            Machine::I386.default_entry(true, false),
            b"_DllMainCRTStartup@12"
        );
        assert_eq!(
            Machine::Arm64.default_entry(false, false),
            b"mainCRTStartup"
        );
        assert_eq!(Machine::I386.optional_header_size(), 224);
    }
}
