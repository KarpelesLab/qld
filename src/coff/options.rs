//! PE-specific link options.
//!
//! [`PeOptions`] is what the PE backend reads. The command line fills it in
//! through [`PeOptions::from_link_options`], which maps
//! [`LinkOptions::pe`](crate::args::LinkOptions::pe) — the MinGW options
//! `--subsystem`, `--out-implib`, `--dynamicbase`, … — together with the
//! shared options that also matter to PE (`-shared`, `--image-base`,
//! `-nostdlib`). Library callers can build a [`PeOptions`] directly instead;
//! every field has a GNU ld `i386pep` default.

use std::path::PathBuf;

use crate::args::{LinkOptions, OutputKind, StripMode};
use crate::error::{Error, Result};

use super::read::consts::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_SUBSYSTEM_NATIVE, IMAGE_SUBSYSTEM_WINDOWS_CUI,
    IMAGE_SUBSYSTEM_WINDOWS_GUI,
};

/// Default image base of a PE32+ executable.
pub const DEFAULT_IMAGE_BASE_EXE: u64 = 0x1_4000_0000;
/// Default image base of a PE32+ DLL.
pub const DEFAULT_IMAGE_BASE_DLL: u64 = 0x1_8000_0000;
/// Default section alignment (the page size).
pub const DEFAULT_SECTION_ALIGNMENT: u32 = 0x1000;
/// Default file alignment.
pub const DEFAULT_FILE_ALIGNMENT: u32 = 0x200;
/// Default stack reserve of a MinGW image.
pub const DEFAULT_STACK_RESERVE: u64 = 0x20_0000;
/// Default stack commit of a MinGW image.
pub const DEFAULT_STACK_COMMIT: u64 = 0x1000;
/// Default heap reserve of a MinGW image.
pub const DEFAULT_HEAP_RESERVE: u64 = 0x10_0000;
/// Default heap commit of a MinGW image.
pub const DEFAULT_HEAP_COMMIT: u64 = 0x1000;

/// A `MAJOR[.MINOR]` version pair in the optional header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Version {
    /// Major version.
    pub major: u16,
    /// Minor version.
    pub minor: u16,
}

impl Version {
    /// A version pair.
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

/// How MinGW auto-import handles references to imported data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AutoImport {
    /// `--disable-auto-import`: a direct reference to imported data is an
    /// error.
    Disabled,
    /// `--enable-auto-import` (the MinGW default): the reference is fixed up
    /// at run time through `__RUNTIME_PSEUDO_RELOC_LIST__`.
    #[default]
    Enabled,
}

/// Everything the PE backend needs to describe an image.
///
/// Every field has a MinGW `ld` default, so
/// [`PeOptions::from_link_options`] on a command line that names no PE
/// option produces the binary `x86_64-w64-mingw32-gcc` expects.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PeOptions {
    /// `Machine` (`IMAGE_FILE_MACHINE_*`) of the output.
    pub machine: u16,
    /// `--dll` / `-shared`: produce a DLL.
    pub dll: bool,
    /// `--image-base`. `None` uses the default for the output kind.
    pub image_base: Option<u64>,
    /// `--section-alignment`.
    pub section_alignment: u32,
    /// `--file-alignment`.
    pub file_alignment: u32,
    /// `--subsystem`. `None` infers it from the entry point.
    pub subsystem: Option<u16>,
    /// `--major-subsystem-version` / `--minor-subsystem-version`.
    pub subsystem_version: Version,
    /// `--major-os-version` / `--minor-os-version`.
    pub os_version: Version,
    /// `--major-image-version` / `--minor-image-version`.
    pub image_version: Version,
    /// `--stack reserve[,commit]`.
    pub stack: (u64, u64),
    /// `--heap reserve[,commit]`.
    pub heap: (u64, u64),
    /// `--dynamicbase`: emit `.reloc` and set `DYNAMIC_BASE`.
    pub dynamicbase: bool,
    /// `--nxcompat`.
    pub nxcompat: bool,
    /// `--high-entropy-va`.
    pub high_entropy_va: bool,
    /// `--tsaware`.
    pub tsaware: bool,
    /// `--no-seh`.
    pub no_seh: bool,
    /// `--forceinteg`.
    pub forceinteg: bool,
    /// `--no-isolation`.
    pub no_isolation: bool,
    /// `--no-bind`.
    pub no_bind: bool,
    /// `--wdmdriver`.
    pub wdmdriver: bool,
    /// `--large-address-aware`.
    pub large_address_aware: bool,
    /// `--disable-reloc-section`: never emit `.reloc`.
    pub disable_reloc_section: bool,
    /// `--insert-timestamp`: use the current time instead of 0.
    pub insert_timestamp: bool,
    /// `--out-implib`: write an import library for the exports.
    pub out_implib: Option<PathBuf>,
    /// `--output-def`: write a `.def` file for the exports.
    pub output_def: Option<PathBuf>,
    /// A `.def` file naming exports and the output name.
    pub def_file: Option<PathBuf>,
    /// `--export-all-symbols`.
    pub export_all_symbols: bool,
    /// `--exclude-all-symbols`.
    pub exclude_all_symbols: bool,
    /// `--exclude-symbols`: names `--export-all-symbols` skips.
    pub exclude_symbols: Vec<Vec<u8>>,
    /// `--kill-at`: drop the `@N` suffix of stdcall names in exports.
    pub kill_at: bool,
    /// `--add-stdcall-alias`: also export the undecorated name.
    pub add_stdcall_alias: bool,
    /// `--enable-stdcall-fixup`: resolve `_foo@8` against `_foo`, and back.
    pub enable_stdcall_fixup: Option<bool>,
    /// `--enable-auto-import` / `--disable-auto-import`.
    pub auto_import: AutoImport,
    /// `--enable-runtime-pseudo-reloc`: emit the version 2 pseudo-relocation
    /// list auto-import needs.
    pub runtime_pseudo_reloc: bool,
    /// `--no-default-lib` / `-nostdlib`: ignore `.drectve` `-defaultlib:`.
    pub no_default_lib: bool,
    /// `-nodefaultlib:name` from `.drectve` and the command line.
    pub no_default_libs: Vec<Vec<u8>>,
    /// `--exclude-modules-for-implib`: objects and archives whose symbols the
    /// import library omits.
    pub exclude_modules_for_implib: Vec<Vec<u8>>,
    /// The import library's `DT_SONAME` equivalent: the DLL name recorded in
    /// it. Defaults to the output file name.
    pub implib_dll_name: Option<Vec<u8>>,
    /// `-export:` specifications from the command line (`.drectve` adds more).
    pub exports: Vec<Vec<u8>>,
    /// `--warn-duplicate-exports`.
    pub warn_duplicate_exports: bool,
}

impl Default for PeOptions {
    fn default() -> Self {
        Self {
            machine: IMAGE_FILE_MACHINE_AMD64,
            dll: false,
            image_base: None,
            section_alignment: DEFAULT_SECTION_ALIGNMENT,
            file_alignment: DEFAULT_FILE_ALIGNMENT,
            subsystem: None,
            // GNU ld's `i386pep` defaults: subsystem 5.02, OS 4.0, image 0.0.
            subsystem_version: Version::new(5, 2),
            os_version: Version::new(4, 0),
            image_version: Version::new(0, 0),
            stack: (DEFAULT_STACK_RESERVE, DEFAULT_STACK_COMMIT),
            heap: (DEFAULT_HEAP_RESERVE, DEFAULT_HEAP_COMMIT),
            dynamicbase: true,
            nxcompat: true,
            high_entropy_va: true,
            tsaware: false,
            no_seh: false,
            forceinteg: false,
            no_isolation: false,
            no_bind: false,
            wdmdriver: false,
            large_address_aware: true,
            disable_reloc_section: false,
            insert_timestamp: false,
            out_implib: None,
            output_def: None,
            def_file: None,
            export_all_symbols: false,
            exclude_all_symbols: false,
            exclude_symbols: Vec::new(),
            kill_at: false,
            add_stdcall_alias: false,
            enable_stdcall_fixup: None,
            auto_import: AutoImport::Enabled,
            runtime_pseudo_reloc: true,
            no_default_lib: false,
            no_default_libs: Vec::new(),
            exclude_modules_for_implib: Vec::new(),
            implib_dll_name: None,
            exports: Vec::new(),
            warn_duplicate_exports: false,
        }
    }
}

impl PeOptions {
    /// The PE options a command line asks for.
    ///
    /// Everything comes from [`LinkOptions`]: the MinGW options from
    /// [`LinkOptions::pe`], `-shared` and `--dll` from
    /// [`kind`](LinkOptions::kind), and `--image-base`, `--export-dynamic`
    /// and `-nostdlib` from the shared options they already had. A field that
    /// no option sets (the import library's DLL name, `-nodefaultlib:` names
    /// and `.drectve` exports) keeps its default for library callers to set.
    #[must_use]
    pub fn from_link_options(options: &LinkOptions) -> Self {
        let pe = &options.pe;
        let dll = options.kind == OutputKind::Shared;
        let names = |list: &[String]| list.iter().map(|name| name.as_bytes().to_vec()).collect();
        Self {
            machine: options
                .target
                .and_then(|target| super::machine_for(target.arch))
                .unwrap_or(IMAGE_FILE_MACHINE_AMD64),
            dll,
            image_base: options.image_base,
            section_alignment: pe.section_alignment,
            file_alignment: pe.file_alignment,
            subsystem: pe.subsystem,
            subsystem_version: Version::new(pe.major_subsystem_version, pe.minor_subsystem_version),
            os_version: Version::new(pe.major_os_version, pe.minor_os_version),
            image_version: Version::new(pe.major_image_version, pe.minor_image_version),
            stack: pe.stack,
            heap: pe.heap,
            dynamicbase: pe.dynamicbase,
            nxcompat: pe.nxcompat,
            high_entropy_va: pe.high_entropy_va,
            tsaware: pe.tsaware,
            no_seh: pe.no_seh,
            forceinteg: pe.forceinteg,
            no_isolation: pe.no_isolation,
            no_bind: pe.no_bind,
            wdmdriver: pe.wdmdriver,
            large_address_aware: pe.large_address_aware,
            disable_reloc_section: !pe.reloc_section,
            insert_timestamp: pe.insert_timestamp,
            out_implib: pe.out_implib.clone(),
            output_def: pe.output_def.clone(),
            def_file: pe.def_file.clone(),
            export_all_symbols: pe.export_all_symbols || (dll && options.export_dynamic),
            exclude_all_symbols: pe.exclude_all_symbols,
            exclude_symbols: names(&pe.exclude_symbols),
            kill_at: pe.kill_at,
            add_stdcall_alias: pe.add_stdcall_alias,
            enable_stdcall_fixup: pe.stdcall_fixup,
            auto_import: if pe.auto_import {
                AutoImport::Enabled
            } else {
                AutoImport::Disabled
            },
            runtime_pseudo_reloc: pe.runtime_pseudo_reloc,
            no_default_lib: options.nostdlib,
            no_default_libs: Vec::new(),
            exclude_modules_for_implib: names(&pe.exclude_modules_for_implib),
            implib_dll_name: None,
            exports: names(&pe.exports),
            warn_duplicate_exports: pe.warn_duplicate_exports,
        }
    }

    /// The image base: `--image-base`, or the default for the output kind.
    #[must_use]
    pub fn effective_image_base(&self) -> u64 {
        self.image_base.unwrap_or(if self.dll {
            DEFAULT_IMAGE_BASE_DLL
        } else {
            DEFAULT_IMAGE_BASE_EXE
        })
    }

    /// Rejects alignments PE cannot express.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Option`] if either alignment is not a power of two, if
    /// the file alignment is outside 512–64 KiB, or if the section alignment
    /// is smaller than the file alignment.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str, value: u64| Error::Option(format!("invalid {what}: {value:#x}"));
        if !self.file_alignment.is_power_of_two()
            || !(512..=0x1_0000).contains(&self.file_alignment)
        {
            return Err(bad("--file-alignment", u64::from(self.file_alignment)));
        }
        if !self.section_alignment.is_power_of_two() {
            return Err(bad(
                "--section-alignment",
                u64::from(self.section_alignment),
            ));
        }
        if self.section_alignment < self.file_alignment {
            return Err(Error::Option(
                "--section-alignment must be at least --file-alignment".into(),
            ));
        }
        if !self.effective_image_base().is_multiple_of(0x1_0000) {
            return Err(bad("--image-base", self.effective_image_base()));
        }
        Ok(())
    }

    /// Whether the output keeps a COFF symbol table (`-s` / `--strip-all`
    /// drops it).
    #[must_use]
    pub fn keep_symbols(options: &LinkOptions) -> bool {
        options.strip < StripMode::All
    }
}

/// The subsystem a `--subsystem` value names, with its optional
/// `,major[.minor]` version.
///
/// # Errors
///
/// Returns [`Error::Option`] for an unknown subsystem name.
pub fn parse_subsystem(value: &str) -> Result<(u16, Option<Version>)> {
    let (name, version) = match value.split_once([',', ':']) {
        Some((name, version)) => (name, Some(version)),
        None => (value, None),
    };
    let subsystem = match name.to_ascii_lowercase().as_str() {
        "console" => IMAGE_SUBSYSTEM_WINDOWS_CUI,
        "windows" => IMAGE_SUBSYSTEM_WINDOWS_GUI,
        "native" => IMAGE_SUBSYSTEM_NATIVE,
        "posix" => 7,
        "wince" => 9,
        "efi-app" | "efi_application" => 10,
        "efi-bsd" | "efi_boot_service_driver" => 11,
        "efi-rtd" | "efi_runtime_driver" => 12,
        "efi-rom" | "efi_rom" => 13,
        "xbox" => 14,
        digits => {
            return digits
                .parse::<u16>()
                .map(|n| (n, None))
                .map_err(|_| Error::Option(format!("unknown subsystem `{name}`")));
        }
    };
    let version = match version {
        None => None,
        Some(text) => {
            let (major, minor) = match text.split_once('.') {
                Some((major, minor)) => (major, Some(minor)),
                None => (text, None),
            };
            let parse = |field: &str| {
                field
                    .parse::<u16>()
                    .map_err(|_| Error::Option(format!("invalid subsystem version `{text}`")))
            };
            Some(Version::new(
                parse(major)?,
                match minor {
                    Some(minor) => parse(minor)?,
                    None => 0,
                },
            ))
        }
    };
    Ok((subsystem, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_mingw() {
        let options = PeOptions::default();
        assert_eq!(options.effective_image_base(), DEFAULT_IMAGE_BASE_EXE);
        let dll = PeOptions {
            dll: true,
            ..PeOptions::default()
        };
        assert_eq!(dll.effective_image_base(), DEFAULT_IMAGE_BASE_DLL);
        options.validate().unwrap();
    }

    #[test]
    fn rejects_bad_alignments() {
        let bad = PeOptions {
            file_alignment: 300,
            ..PeOptions::default()
        };
        assert!(bad.validate().is_err());
        let bad = PeOptions {
            section_alignment: 256,
            file_alignment: 512,
            ..PeOptions::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn command_line_defaults_match_the_mingw_defaults() {
        // A command line that names no PE option must describe exactly the
        // image `PeOptions::default()` does, so that the option table and
        // this module cannot drift apart.
        let options = LinkOptions::new();
        assert_eq!(PeOptions::from_link_options(&options), PeOptions::default());
    }

    #[test]
    fn shared_output_becomes_a_dll() {
        let options = LinkOptions {
            kind: OutputKind::Shared,
            export_dynamic: true,
            nostdlib: true,
            ..LinkOptions::new()
        };
        let pe = PeOptions::from_link_options(&options);
        assert!(pe.dll && pe.export_all_symbols && pe.no_default_lib);
        assert_eq!(pe.effective_image_base(), DEFAULT_IMAGE_BASE_DLL);
    }

    #[test]
    fn subsystems() {
        assert_eq!(
            parse_subsystem("console").unwrap(),
            (IMAGE_SUBSYSTEM_WINDOWS_CUI, None)
        );
        assert_eq!(
            parse_subsystem("windows,6.1").unwrap(),
            (IMAGE_SUBSYSTEM_WINDOWS_GUI, Some(Version::new(6, 1)))
        );
        assert_eq!(parse_subsystem("10").unwrap(), (10, None));
        assert!(parse_subsystem("bogus").is_err());
    }
}
