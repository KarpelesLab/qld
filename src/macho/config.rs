//! The settings of one architecture's link, derived from [`LinkOptions`].

#![deny(clippy::arithmetic_side_effects)]

use std::path::PathBuf;

use crate::args::LinkOptions;
use crate::args::darwin::{MachOutputType, PlatformVersion, UndefinedTreatment, UuidMode};
use crate::error::{Error, Result};
use crate::macho::read::Arch;
use crate::macho::read::commands::PackedVersion;
use crate::macho::read::consts::{
    CPU_TYPE_ARM64, CPU_TYPE_X86_64, PLATFORM_DRIVERKIT, PLATFORM_IOS, PLATFORM_IOSSIMULATOR,
    PLATFORM_MACCATALYST, PLATFORM_MACOS, PLATFORM_TVOS, PLATFORM_TVOSSIMULATOR, PLATFORM_WATCHOS,
    PLATFORM_WATCHOSSIMULATOR, PLATFORM_XROS, PLATFORM_XROS_SIMULATOR,
};

/// Everything one slice's link needs to know.
#[derive(Clone, Debug)]
pub struct Config {
    /// The architecture being linked.
    pub arch: Arch,
    /// What kind of file is produced.
    pub output_type: MachOutputType,
    /// Platform and versions for `LC_BUILD_VERSION`.
    pub platform: PlatformVersion,
    /// Segment alignment: 16 KiB on arm64, 4 KiB on x86_64.
    pub page_size: u64,
    /// `LC_DYLD_CHAINED_FIXUPS` rather than `LC_DYLD_INFO_ONLY`.
    pub chained_fixups: bool,
    /// `__DATA_CONST` is used for pointers made read-only after fixups.
    pub data_const: bool,
    /// Size of `__PAGEZERO` (executables only; 0 means none).
    pub pagezero: u64,
    /// Address of the Mach-O header.
    pub image_base: u64,
    /// Bytes reserved after the load commands.
    pub headerpad: u64,
    /// Leave room to rewrite every dylib path to `MAXPATHLEN`.
    pub headerpad_max_install_names: bool,
    /// Write an ad-hoc code signature.
    pub sign: bool,
    /// The entry symbol (executables).
    pub entry: Option<Vec<u8>>,
    /// Install name (dylibs).
    pub install_name: Vec<u8>,
    /// Current version (dylibs).
    pub current_version: PackedVersion,
    /// Compatibility version (dylibs).
    pub compatibility_version: PackedVersion,
    /// `-dead_strip`.
    pub dead_strip: bool,
    /// `-undefined`.
    pub undefined: UndefinedTreatment,
    /// Write `LC_UUID`.
    pub uuid: bool,
    /// The output file name, used as the code signature identifier.
    pub identifier: Vec<u8>,
    /// Write the STABS debug map.
    pub debug_map: bool,
    /// `-oso_prefix`.
    pub oso_prefix: Option<PathBuf>,
}

/// The first deployment target of each platform where ld64 defaults to
/// chained fixups.
fn chained_fixups_by_default(platform: &PlatformVersion) -> bool {
    let min = match platform.platform {
        PLATFORM_MACOS => PackedVersion::new(12, 0, 0),
        PLATFORM_IOS | PLATFORM_TVOS | PLATFORM_MACCATALYST => PackedVersion::new(15, 0, 0),
        PLATFORM_IOSSIMULATOR | PLATFORM_TVOSSIMULATOR => PackedVersion::new(15, 0, 0),
        PLATFORM_WATCHOS | PLATFORM_WATCHOSSIMULATOR => PackedVersion::new(8, 0, 0),
        PLATFORM_XROS | PLATFORM_XROS_SIMULATOR | PLATFORM_DRIVERKIT => PackedVersion::new(1, 0, 0),
        _ => return false,
    };
    platform.min.0 >= min.0
}

/// Whether `__DATA_CONST` is used, as in lld: macOS 10.15, iOS 13 and later.
fn data_const_by_default(platform: &PlatformVersion) -> bool {
    let min = match platform.platform {
        PLATFORM_MACOS => PackedVersion::new(10, 15, 0),
        PLATFORM_IOS | PLATFORM_IOSSIMULATOR | PLATFORM_TVOS | PLATFORM_TVOSSIMULATOR => {
            PackedVersion::new(13, 0, 0)
        }
        PLATFORM_WATCHOS | PLATFORM_WATCHOSSIMULATOR => PackedVersion::new(6, 0, 0),
        _ => PackedVersion::new(0, 0, 0),
    };
    platform.min.0 >= min.0
}

impl Config {
    /// Derives the configuration for `arch`.
    ///
    /// `inferred_platform` is the build version of the first object, used
    /// when the command line has no `-platform_version`.
    ///
    /// # Errors
    ///
    /// [`Error::Unimplemented`] for architectures other than arm64 and
    /// x86_64, and [`Error::Option`] for inconsistent options.
    pub fn new(
        options: &LinkOptions,
        arch: Arch,
        inferred_platform: Option<PlatformVersion>,
    ) -> Result<Self> {
        let darwin = &options.darwin;
        let is_arm64 = match arch.cpu_type {
            CPU_TYPE_ARM64 => true,
            CPU_TYPE_X86_64 => false,
            _ => {
                return Err(Error::Unimplemented(format!(
                    "Mach-O output for architecture {arch} (roadmap M8 covers arm64 and x86_64)"
                )));
            }
        };
        if arch == Arch::ARM64E {
            return Err(Error::Unimplemented(
                "arm64e output (pointer authentication; roadmap M8 covers arm64)".into(),
            ));
        }
        let platform = darwin
            .platform
            .or(inferred_platform)
            .unwrap_or(PlatformVersion {
                platform: PLATFORM_MACOS,
                min: if is_arm64 {
                    PackedVersion::new(11, 0, 0)
                } else {
                    PackedVersion::new(10, 15, 0)
                },
                sdk: PackedVersion::new(11, 0, 0),
            });
        let output_type = darwin.output_type;
        let is_exec = output_type == MachOutputType::Execute;
        if darwin.pie == Some(false) && is_arm64 && is_exec {
            return Err(Error::Option(
                "-no_pie is not supported for arm64 executables".into(),
            ));
        }
        if darwin.pie == Some(false) && is_exec {
            return Err(Error::Unimplemented(
                "non-PIE Mach-O executables (-no_pie)".into(),
            ));
        }
        let page_size = if is_arm64 { 0x4000 } else { 0x1000 };
        let pagezero = if is_exec {
            darwin.pagezero_size.unwrap_or(0x1_0000_0000)
        } else {
            0
        };
        if pagezero.checked_rem(page_size) != Some(0) {
            return Err(Error::Option(format!(
                "-pagezero_size {pagezero:#x} is not a multiple of the page size {page_size:#x}"
            )));
        }
        let image_base = darwin.image_base.unwrap_or(pagezero);
        if is_exec && image_base < pagezero {
            return Err(Error::Option(format!(
                "-image_base {image_base:#x} is inside __PAGEZERO"
            )));
        }
        let entry = is_exec.then(|| {
            options
                .entry
                .as_deref()
                .unwrap_or("_main")
                .as_bytes()
                .to_vec()
        });
        let output = options.output_path();
        let identifier = output
            .file_name()
            .map_or_else(|| b"a.out".to_vec(), |n| n.as_encoded_bytes().to_vec());
        let install_name = match (&options.soname, output_type) {
            (Some(name), _) => name.as_bytes().to_vec(),
            (None, MachOutputType::Dylib) => output.as_os_str().as_encoded_bytes().to_vec(),
            (None, _) => Vec::new(),
        };
        Ok(Self {
            arch,
            output_type,
            platform,
            page_size,
            chained_fixups: darwin
                .fixup_chains
                .unwrap_or_else(|| chained_fixups_by_default(&platform)),
            data_const: data_const_by_default(&platform),
            pagezero,
            image_base,
            headerpad: darwin.headerpad.unwrap_or(32),
            headerpad_max_install_names: darwin.headerpad_max_install_names,
            sign: darwin.adhoc_codesign.unwrap_or(is_arm64),
            entry,
            install_name,
            current_version: darwin
                .current_version
                .unwrap_or(PackedVersion::new(1, 0, 0)),
            compatibility_version: darwin
                .compatibility_version
                .unwrap_or(PackedVersion::new(1, 0, 0)),
            dead_strip: options.gc_sections,
            undefined: darwin.undefined,
            uuid: darwin.uuid == UuidMode::Content,
            identifier,
            debug_map: options.strip == crate::args::StripMode::None,
            oso_prefix: darwin.oso_prefix.clone(),
        })
    }

    /// Whether the output is arm64.
    #[must_use]
    pub fn is_arm64(&self) -> bool {
        self.arch.cpu_type == CPU_TYPE_ARM64
    }

    /// Whether the output is an executable.
    #[must_use]
    pub fn is_exec(&self) -> bool {
        self.output_type == MachOutputType::Execute
    }

    /// Size of `__stubs` entries.
    #[must_use]
    pub fn stub_size(&self) -> u64 {
        if self.is_arm64() { 12 } else { 6 }
    }
}
