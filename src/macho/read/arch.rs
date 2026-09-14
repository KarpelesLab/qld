//! CPU type and subtype pairs, and their names (`arm64`, `x86_64h`, …).

use std::fmt;

use super::consts::{
    CPU_ARCH_ABI64, CPU_SUBTYPE_ARM_V6, CPU_SUBTYPE_ARM_V6M, CPU_SUBTYPE_ARM_V7,
    CPU_SUBTYPE_ARM_V7EM, CPU_SUBTYPE_ARM_V7K, CPU_SUBTYPE_ARM_V7M, CPU_SUBTYPE_ARM_V7S,
    CPU_SUBTYPE_ARM64_32_V8, CPU_SUBTYPE_ARM64_ALL, CPU_SUBTYPE_ARM64E, CPU_SUBTYPE_I386_ALL,
    CPU_SUBTYPE_MASK, CPU_SUBTYPE_POWERPC_ALL, CPU_SUBTYPE_X86_64_ALL, CPU_SUBTYPE_X86_64_H,
    CPU_TYPE_ARM, CPU_TYPE_ARM64, CPU_TYPE_ARM64_32, CPU_TYPE_POWERPC, CPU_TYPE_POWERPC64,
    CPU_TYPE_X86, CPU_TYPE_X86_64,
};
use crate::target::Architecture;

/// A Mach-O architecture: `cputype` plus `cpusubtype` without its
/// capability bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Arch {
    /// `cputype`.
    pub cpu_type: u32,
    /// `cpusubtype & !CPU_SUBTYPE_MASK`.
    pub cpu_subtype: u32,
}

/// Names of the architectures qld knows, as `-arch` and `.tbd` files spell
/// them.
const NAMES: &[(&str, u32, u32)] = &[
    ("arm64", CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_ALL),
    ("arm64e", CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64E),
    ("arm64_32", CPU_TYPE_ARM64_32, CPU_SUBTYPE_ARM64_32_V8),
    ("x86_64", CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_ALL),
    ("x86_64h", CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_H),
    ("i386", CPU_TYPE_X86, CPU_SUBTYPE_I386_ALL),
    ("armv7", CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V7),
    ("armv7s", CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V7S),
    ("armv7k", CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V7K),
    ("armv6", CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V6),
    ("armv6m", CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V6M),
    ("armv7m", CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V7M),
    ("armv7em", CPU_TYPE_ARM, CPU_SUBTYPE_ARM_V7EM),
    ("ppc", CPU_TYPE_POWERPC, CPU_SUBTYPE_POWERPC_ALL),
    ("ppc64", CPU_TYPE_POWERPC64, CPU_SUBTYPE_POWERPC_ALL),
];

impl Arch {
    /// arm64.
    pub const ARM64: Self = Self::new(CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_ALL);
    /// arm64e.
    pub const ARM64E: Self = Self::new(CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64E);
    /// x86_64.
    pub const X86_64: Self = Self::new(CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_ALL);
    /// x86_64h (Haswell).
    pub const X86_64H: Self = Self::new(CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_H);
    /// i386.
    pub const I386: Self = Self::new(CPU_TYPE_X86, CPU_SUBTYPE_I386_ALL);

    /// Builds an architecture, masking the capability bits of `cpu_subtype`.
    #[must_use]
    pub const fn new(cpu_type: u32, cpu_subtype: u32) -> Self {
        Self {
            cpu_type,
            cpu_subtype: cpu_subtype & !CPU_SUBTYPE_MASK,
        }
    }

    /// Looks up an architecture by name (`arm64`, `x86_64`, …).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        NAMES
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|&(_, cpu_type, cpu_subtype)| Self::new(cpu_type, cpu_subtype))
    }

    /// The name of this architecture, when qld knows it.
    #[must_use]
    pub fn name(self) -> Option<&'static str> {
        NAMES
            .iter()
            .find(|&&(_, t, s)| t == self.cpu_type && s == self.cpu_subtype)
            .map(|(n, _, _)| *n)
    }

    /// Whether the CPU type has the 64-bit ABI flag.
    #[must_use]
    pub fn is_64bit(self) -> bool {
        self.cpu_type & CPU_ARCH_ABI64 != 0
    }

    /// The qld [`Architecture`] of this CPU type.
    #[must_use]
    pub fn architecture(self) -> Option<Architecture> {
        Some(match self.cpu_type {
            CPU_TYPE_X86_64 => Architecture::X86_64,
            CPU_TYPE_X86 => Architecture::X86,
            CPU_TYPE_ARM64 => Architecture::Aarch64,
            CPU_TYPE_ARM => Architecture::Arm,
            CPU_TYPE_POWERPC64 => Architecture::PowerPc64,
            _ => return None,
        })
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(
                f,
                "cputype {:#x} cpusubtype {:#x}",
                self.cpu_type, self.cpu_subtype
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for &(name, _, _) in NAMES {
            let arch = Arch::from_name(name).unwrap();
            assert_eq!(arch.name(), Some(name));
            assert_eq!(arch.to_string(), name);
        }
        assert_eq!(Arch::new(CPU_TYPE_ARM64, 0x8000_0002), Arch::ARM64E);
        assert_eq!(Arch::new(99, 1).to_string(), "cputype 0x63 cpusubtype 0x1");
        assert!(Arch::ARM64.is_64bit());
        assert!(!Arch::I386.is_64bit());
    }
}
