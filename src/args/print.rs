//! What `--print-output-format` and `--print-sysroot` print, as GNU ld
//! prints it.
//!
//! The two options are still rejected by the parser: printing and exiting
//! needs a [`ParseOutcome`](super::ParseOutcome) variant that the `qld`
//! binary handles, and the binary is a frozen file. These functions are the
//! text the binary will print once it does:
//!
//! - `--print-sysroot` prints the sysroot and exits as soon as GNU ld
//!   reads the option, whatever follows; the sysroot is the last
//!   `--sysroot=DIR` anywhere on the (response-file-expanded) command line,
//!   since GNU ld scans for it before parsing ([`sysroot_text`]).
//! - `--print-output-format` prints the BFD name of the output format once
//!   the command line is parsed, and then links if there are inputs, or
//!   exits successfully if there are none ([`output_format_name`]).

use crate::target::{Architecture, BinaryFormat, Endianness, OperatingSystem, Target};

use super::options::LinkOptions;

/// What `--print-sysroot` prints for the expanded command line `args`
/// (without `argv[0]`): the last `--sysroot=DIR` and a newline, or nothing
/// at all when there is none, as a GNU ld configured without a sysroot.
///
/// Like GNU ld's `get_sysroot`, only the `--sysroot=DIR` spelling counts.
#[must_use]
pub fn sysroot_text(args: &[Vec<u8>]) -> Vec<u8> {
    let Some(sysroot) = args
        .iter()
        .rev()
        .find_map(|arg| arg.strip_prefix(b"--sysroot="))
    else {
        return Vec::new();
    };
    if sysroot.is_empty() {
        return Vec::new();
    }
    let mut text = sysroot.to_vec();
    text.push(b'\n');
    text
}

/// What `--print-output-format` prints (without the newline): the
/// `--oformat` name if one was given, else the BFD target name of the `-m`
/// emulation (with `-EB`/`-EL` picking the byte order of a bi-endian
/// architecture), else that of the default target.
///
/// An `OUTPUT_FORMAT` in a `-T` script, which GNU ld also reports, is not
/// known until the script is read, so it is not seen here.
#[must_use]
pub fn output_format_name(options: &LinkOptions) -> String {
    if let Some(format) = &options.output_format {
        return format.name().to_owned();
    }
    let mut target = options
        .target
        .unwrap_or_else(crate::elf::target::default_target);
    if let Some(endian) = options.endian
        && bi_endian(target.arch)
    {
        target.endian = endian;
    }
    bfd_target_name(target).to_owned()
}

/// Whether `-EB`/`-EL` select the byte order on `arch`, as they do for
/// GNU ld's bi-endian targets; elsewhere they are ignored.
fn bi_endian(arch: Architecture) -> bool {
    matches!(
        arch,
        Architecture::Aarch64
            | Architecture::Arm
            | Architecture::Riscv64
            | Architecture::Riscv32
            | Architecture::PowerPc64
    )
}

/// BFD's name for the default output format of `target`
/// (`elf64-x86-64`, `pei-i386`, …).
#[must_use]
pub fn bfd_target_name(target: Target) -> &'static str {
    use Architecture as A;
    let big = target.endian == Endianness::Big;
    match target.format {
        BinaryFormat::Pe => match target.arch {
            A::X86 => "pei-i386",
            A::Aarch64 => "pei-aarch64-little",
            A::Arm => "pei-arm-little",
            _ => "pei-x86-64",
        },
        BinaryFormat::MachO => match target.arch {
            A::Aarch64 => "mach-o-arm64",
            _ => "mach-o-x86-64",
        },
        BinaryFormat::Binary => "binary",
        BinaryFormat::Hunk => "amiga",
        BinaryFormat::Elf => match target.arch {
            A::X86_64 => "elf64-x86-64",
            A::X86_64X32 => "elf32-x86-64",
            A::X86 if target.os == OperatingSystem::None => "elf32-iamcu",
            A::X86 => "elf32-i386",
            A::Aarch64 if big => "elf64-bigaarch64",
            A::Aarch64 => "elf64-littleaarch64",
            A::Arm if big => "elf32-bigarm",
            A::Arm => "elf32-littlearm",
            A::Riscv64 if big => "elf64-bigriscv",
            A::Riscv64 => "elf64-littleriscv",
            A::Riscv32 if big => "elf32-bigriscv",
            A::Riscv32 => "elf32-littleriscv",
            A::PowerPc64 if big => "elf64-powerpc",
            A::PowerPc64 => "elf64-powerpcle",
            A::LoongArch64 => "elf64-loongarch",
            A::S390x => "elf64-s390",
            A::M68k => "elf32-m68k",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::emulation;
    use crate::args::options::OutputFormat;

    fn args(list: &[&str]) -> Vec<Vec<u8>> {
        list.iter().map(|a| a.as_bytes().to_vec()).collect()
    }

    /// `ld.bfd --print-sysroot` (GNU ld 2.46, no configured sysroot).
    #[test]
    fn sysroot_as_gnu_ld_prints_it() {
        assert_eq!(sysroot_text(&args(&["--print-sysroot"])), b"");
        assert_eq!(
            sysroot_text(&args(&["--sysroot=/foo", "--print-sysroot"])),
            b"/foo\n"
        );
        // Found before parsing: a later one counts too, the last one wins.
        assert_eq!(
            sysroot_text(&args(&["--sysroot=/a", "--print-sysroot", "--sysroot=/b"])),
            b"/b\n"
        );
        assert_eq!(
            sysroot_text(&args(&["--sysroot", "/a", "--print-sysroot"])),
            b""
        );
    }

    /// `ld.bfd --print-output-format` with the same options (GNU ld 2.46
    /// for x86-64, `aarch64-unknown-linux-gnu-ld`, the MinGW linkers).
    #[test]
    fn output_formats_as_gnu_ld_prints_them() {
        let name = |emulation: &str, endian: Option<Endianness>| {
            let mut options = LinkOptions::new();
            options.target = emulation::lookup(emulation);
            options.endian = endian;
            output_format_name(&options)
        };
        assert_eq!(name("elf_x86_64", None), "elf64-x86-64");
        assert_eq!(name("elf_i386", None), "elf32-i386");
        assert_eq!(name("elf32_x86_64", None), "elf32-x86-64");
        assert_eq!(name("elf_iamcu", None), "elf32-iamcu");
        assert_eq!(name("elf_x86_64", Some(Endianness::Big)), "elf64-x86-64");
        assert_eq!(name("aarch64linux", None), "elf64-littleaarch64");
        assert_eq!(
            name("aarch64linux", Some(Endianness::Big)),
            "elf64-bigaarch64"
        );
        assert_eq!(name("i386pep", None), "pei-x86-64");
        assert_eq!(name("i386pe", None), "pei-i386");

        let mut options = LinkOptions::new();
        options.output_format = Some(OutputFormat::Binary);
        assert_eq!(output_format_name(&options), "binary");
    }
}
