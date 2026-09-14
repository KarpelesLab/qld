//! `-m <emulation>` names and the targets they select.

use crate::target::{
    Architecture, BinaryFormat, Endianness, OperatingSystem, PointerWidth, Target,
};

const fn target(
    format: BinaryFormat,
    arch: Architecture,
    endian: Endianness,
    pointer_width: PointerWidth,
    os: OperatingSystem,
) -> Target {
    Target {
        format,
        arch,
        endian,
        pointer_width,
        os,
    }
}

use Architecture as A;
use BinaryFormat::{Elf, Pe};
use Endianness::{Big, Little};
use OperatingSystem::{Linux, None as Bare, Windows};
use PointerWidth::{Bits32, Bits64};

/// Every emulation qld accepts, with the target it selects.
///
/// Names follow GNU ld (`ld -V` lists them); lld accepts the same spellings.
#[rustfmt::skip]
pub const EMULATIONS: &[(&str, Target)] = &[
    ("elf_x86_64",         target(Elf, A::X86_64, Little, Bits64, Linux)),
    ("elf32_x86_64",       target(Elf, A::X86_64X32, Little, Bits32, Linux)),
    ("elf_i386",           target(Elf, A::X86, Little, Bits32, Linux)),
    ("elf_iamcu",          target(Elf, A::X86, Little, Bits32, Bare)),
    ("aarch64linux",       target(Elf, A::Aarch64, Little, Bits64, Linux)),
    ("aarch64linuxb",      target(Elf, A::Aarch64, Big, Bits64, Linux)),
    ("aarch64elf",         target(Elf, A::Aarch64, Little, Bits64, Bare)),
    ("aarch64elfb",        target(Elf, A::Aarch64, Big, Bits64, Bare)),
    ("armelf_linux_eabi",  target(Elf, A::Arm, Little, Bits32, Linux)),
    ("armelfb_linux_eabi", target(Elf, A::Arm, Big, Bits32, Linux)),
    ("armelf",             target(Elf, A::Arm, Little, Bits32, Bare)),
    ("armelfb",            target(Elf, A::Arm, Big, Bits32, Bare)),
    ("elf64lriscv",        target(Elf, A::Riscv64, Little, Bits64, Linux)),
    ("elf64lriscv_lp64",   target(Elf, A::Riscv64, Little, Bits64, Linux)),
    ("elf64lriscv_lp64f",  target(Elf, A::Riscv64, Little, Bits64, Linux)),
    ("elf64briscv",        target(Elf, A::Riscv64, Big, Bits64, Linux)),
    ("elf32lriscv",        target(Elf, A::Riscv32, Little, Bits32, Linux)),
    ("elf32lriscv_ilp32",  target(Elf, A::Riscv32, Little, Bits32, Linux)),
    ("elf32lriscv_ilp32f", target(Elf, A::Riscv32, Little, Bits32, Linux)),
    ("elf32briscv",        target(Elf, A::Riscv32, Big, Bits32, Linux)),
    ("elf64lppc",          target(Elf, A::PowerPc64, Little, Bits64, Linux)),
    ("elf64ppc",           target(Elf, A::PowerPc64, Big, Bits64, Linux)),
    ("elf64loongarch",     target(Elf, A::LoongArch64, Little, Bits64, Linux)),
    ("elf64_s390",         target(Elf, A::S390x, Big, Bits64, Linux)),
    ("i386pep",            target(Pe, A::X86_64, Little, Bits64, Windows)),
    ("i386pe",             target(Pe, A::X86, Little, Bits32, Windows)),
    ("arm64pe",            target(Pe, A::Aarch64, Little, Bits64, Windows)),
    ("thumb2pe",           target(Pe, A::Arm, Little, Bits32, Windows)),
];

/// Looks up an emulation name.
#[must_use]
pub fn lookup(name: &str) -> Option<Target> {
    EMULATIONS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, target)| *target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique() {
        for (i, (name, _)) in EMULATIONS.iter().enumerate() {
            assert!(
                EMULATIONS
                    .iter()
                    .skip(i + 1)
                    .all(|(other, _)| other != name),
                "duplicate emulation {name}"
            );
        }
    }

    #[test]
    fn common_emulations_resolve() {
        assert_eq!(lookup("elf_x86_64"), Some(Target::X86_64_LINUX));
        assert_eq!(lookup("aarch64linux"), Some(Target::AARCH64_LINUX));
        assert_eq!(lookup("i386pep").map(|t| t.format), Some(BinaryFormat::Pe));
        assert_eq!(lookup("elf_x86_64_fbsd"), None);
    }
}
