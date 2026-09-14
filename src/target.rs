//! Target description: the format, architecture and ABI being linked for.

use std::fmt;

/// Container format of the output.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinaryFormat {
    /// ELF (Linux, BSD, bare metal, …).
    Elf,
    /// PE/COFF (Windows).
    Pe,
    /// Mach-O (macOS, iOS, …).
    MachO,
    /// A flat image with no container, produced by `--oformat binary`.
    Binary,
}

/// Processor architecture.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Architecture {
    /// x86-64 (AMD64).
    X86_64,
    /// x86-64 with 32-bit pointers (x32).
    X86_64X32,
    /// 32-bit x86.
    X86,
    /// 64-bit Arm.
    Aarch64,
    /// 32-bit Arm.
    Arm,
    /// 64-bit RISC-V.
    Riscv64,
    /// 32-bit RISC-V.
    Riscv32,
    /// 64-bit PowerPC.
    PowerPc64,
    /// 64-bit LoongArch.
    LoongArch64,
    /// IBM z/Architecture.
    S390x,
}

/// Byte order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Endianness {
    /// Least significant byte first.
    Little,
    /// Most significant byte first.
    Big,
}

/// Native pointer size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PointerWidth {
    /// 32-bit pointers.
    Bits32,
    /// 64-bit pointers.
    Bits64,
}

/// Operating system and ABI the output runs on.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OperatingSystem {
    /// Linux, any libc.
    Linux,
    /// Apple platforms.
    Darwin,
    /// Windows (MinGW or MSVC environment).
    Windows,
    /// No operating system: firmware, kernels, bare metal.
    None,
}

/// A complete target description.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    /// Output container format.
    pub format: BinaryFormat,
    /// Processor architecture.
    pub arch: Architecture,
    /// Byte order.
    pub endian: Endianness,
    /// Pointer size.
    pub pointer_width: PointerWidth,
    /// Operating system.
    pub os: OperatingSystem,
}

impl Target {
    /// x86-64 Linux ELF.
    pub const X86_64_LINUX: Self = Self {
        format: BinaryFormat::Elf,
        arch: Architecture::X86_64,
        endian: Endianness::Little,
        pointer_width: PointerWidth::Bits64,
        os: OperatingSystem::Linux,
    };

    /// AArch64 Linux ELF.
    pub const AARCH64_LINUX: Self = Self {
        format: BinaryFormat::Elf,
        arch: Architecture::Aarch64,
        endian: Endianness::Little,
        pointer_width: PointerWidth::Bits64,
        os: OperatingSystem::Linux,
    };

    /// Whether addresses are 64 bits wide.
    #[must_use]
    pub fn is_64bit(self) -> bool {
        matches!(self.pointer_width, PointerWidth::Bits64)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}-{:?}-{:?}", self.arch, self.os, self.format)
    }
}
