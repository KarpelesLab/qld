//! Per-architecture relocation handling.
//!
//! Each architecture module classifies relocation types, rewrites relaxable
//! instruction sequences and writes linker-generated stubs. The rest of the
//! ELF backend never names a relocation constant: it asks [`Arch`], which is
//! chosen once per link ([`Arch::of`]) and dispatches to the module for
//! x86-64 (and x32), AArch64, RISC-V 64, LoongArch64, PowerPC64 or i386.
//!
//! The vocabulary is shared, so the relocation scan and the writer run one
//! loop for every architecture:
//!
//! - [`Kind`] says what a relocation computes (`S + A`, `Page(S + A) -
//!   Page(P)`, a GOT slot's address, a thread pointer offset, …) and
//!   [`GotKind`] which GOT entry it goes through;
//! - [`Width`] says how the result is stored: a data field, or an AArch64
//!   RISC-V, LoongArch or PowerPC64 instruction field
//!   ([`crate::arch::aarch64::Field`], [`crate::arch::riscv::Field`],
//!   [`crate::arch::loongarch::Field`], [`crate::arch::ppc64::Field`]);
//! - [`DynKind`] names the dynamic relocation a GOT slot, a PLT slot or a
//!   copy needs, without naming its number.

pub mod aarch64;
pub mod aarch64_errata;
pub mod i386;
pub mod loongarch;
pub mod ppc64;
pub mod riscv;
pub mod shrink;
pub mod thunk;
pub mod x86_64;

use crate::args::LinkOptions;
use crate::elf::read::Relocation;
use crate::elf::read::consts::{
    EM_AARCH64, EM_LOONGARCH, EM_PPC64, EM_RISCV, EM_X86_64, SHF_EXECINSTR, reloc_name,
};
use crate::target::{Architecture, Target};

/// The architecture an ELF link targets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Arch {
    /// x86-64.
    #[default]
    X86_64,
    /// AArch64 (LP64, little-endian).
    AArch64,
    /// RISC-V 64 (LP64, little-endian).
    RiscV64,
    /// LoongArch64 (LP64D, little-endian).
    LoongArch64,
    /// PowerPC64, little-endian, ELFv2 ABI.
    Ppc64,
    /// i386 (32-bit x86).
    I386,
    /// x32: x86-64 with 32-bit pointers (ILP32), in ELF32 objects.
    X32,
}

/// What a relocation computes.
///
/// `S` is the symbol's address, `A` the addend, `P` the place being
/// relocated, `G` the address of the symbol's GOT entry (of the
/// relocation's [`GotKind`]), `GOT` the GOT base, `TP` the thread pointer
/// and `Page(x)` the address with its low 12 bits cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Nothing to do.
    None,
    /// `S + A`.
    Abs,
    /// `S + A - P`.
    Pc,
    /// `Page(S + A) - Page(P)`.
    Page,
    /// `G + A - P`.
    Got,
    /// `Page(G + A) - Page(P)`.
    GotPage,
    /// `G + A`.
    GotAbs,
    /// `G + A - Page(GOT)`.
    GotPageOff,
    /// `G + A - GOT`.
    GotSlotRel,
    /// `S + A - GOT`.
    GotRel,
    /// `GOT + A - P`.
    GotBasePc,
    /// `Z + A`, the symbol's size.
    Size,
    /// `S + A - TP`.
    TpOff,
    /// `S + A - TLS block start` in non-allocated sections, `S + A - TP` in
    /// allocated ones (local-dynamic code relaxed to local-exec).
    DtpOff,
    /// x86-64: a relaxed `GOTPCRELX`, `S + A - P` with the instruction
    /// rewritten.
    RelaxGotPc,
    /// x86-64: a relaxed `REX_GOTPCRELX` to an immediate operand, `S`; i386:
    /// a relaxed `GOT32X` load without a base register, `S + A`.
    RelaxGotPcNoPic,
    /// i386: a relaxed `GOT32X` load through a base register, turned into
    /// a `lea` of `S + A - GOT`.
    RelaxGotOff,
    /// General-dynamic → local-exec.
    GdToLe,
    /// Local-dynamic → local-exec.
    LdToLe,
    /// Initial-exec → local-exec.
    IeToLe,
    /// TLS descriptor → local-exec.
    DescToLe,
    /// A TLS descriptor call that becomes a no-op.
    DescCallToLe,
    /// General-dynamic → initial-exec.
    GdToIe,
    /// TLS descriptor → initial-exec.
    DescToIe,
    /// `S + A` added to the field's current contents (a label difference
    /// assembled as a pair of relocations: LoongArch `ADD*`).
    Add,
    /// `S + A` subtracted from the field's current contents (the other half
    /// of the pair: LoongArch `SUB*`).
    Sub,
    /// The low bits of `S + A` that complete a page-relative pair
    /// (LoongArch `pcalau12i` + `addi.d`): the same wherever the output is
    /// loaded, so it needs no dynamic relocation even in a PIE.
    PageOff,
    /// An instruction pair the architecture rewrites once it knows both
    /// `S + A` and the place ([`Arch::relax`]; LoongArch: a GOT load turned
    /// into an address computation, or linker relaxation to `pcaddi`).
    Relax,
    /// `A` alone: a hint whose addend locates a related instruction
    /// (PowerPC64 `R_PPC64_PCREL_OPT`).
    Addend,
}

/// What a GOT entry holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GotKind {
    /// The symbol's address (one word).
    #[default]
    Address,
    /// TLS module ID and offset (two words).
    TlsGd,
    /// The thread pointer offset (one word).
    TpOff,
    /// A TLS descriptor (two words).
    TlsDesc,
    /// The module-local TLS module ID and a zero offset (two words).
    TlsLd,
}

/// How the computed value is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// Nothing is written.
    None,
    /// 8 bytes.
    W64,
    /// 4 bytes, zero-extended by the processor.
    U32,
    /// 4 bytes, sign-extended.
    I32,
    /// 4 bytes, signed or unsigned.
    Any32,
    /// 2 bytes, signed or unsigned.
    Any16,
    /// 2 bytes, signed.
    I16,
    /// 1 byte, signed or unsigned.
    Any8,
    /// 1 byte, signed.
    I8,
    /// An AArch64 instruction (or data) field.
    Field(crate::arch::aarch64::Field),
    /// A RISC-V instruction or data field, including the label arithmetic
    /// fields that combine with their contents.
    RiscV(crate::arch::riscv::Field),
    /// A LoongArch instruction (or data) field.
    LoongArch(crate::arch::loongarch::Field),
    /// A PowerPC64 instruction (or data) field.
    Ppc(crate::arch::ppc64::Field),
}

/// A classified relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Class {
    /// What to compute.
    pub kind: Kind,
    /// How to store it.
    pub width: Width,
    /// Which GOT entry the `Got*` kinds use.
    pub slot: GotKind,
    /// The relocation that follows is part of this one's sequence and must
    /// not be processed again (the call a TLS relaxation removes).
    pub skip_next: bool,
}

impl Class {
    /// A classification with no GOT entry and no skipped successor.
    #[must_use]
    pub const fn new(kind: Kind, width: Width) -> Self {
        Self {
            kind,
            width,
            slot: GotKind::Address,
            skip_next: false,
        }
    }

    /// The same classification through GOT entry `slot`.
    #[must_use]
    pub const fn through(mut self, slot: GotKind) -> Self {
        self.slot = slot;
        self
    }

    /// The same classification, consuming the relocation that follows.
    #[must_use]
    pub const fn skipping(mut self) -> Self {
        self.skip_next = true;
        self
    }

    /// Whether the relocation reads a GOT entry, and so needs one.
    #[must_use]
    pub fn needs_got(self) -> bool {
        matches!(
            self.kind,
            Kind::Got | Kind::GotPage | Kind::GotAbs | Kind::GotPageOff | Kind::GotSlotRel
        )
    }

    /// Whether the relocation uses the GOT base, so `_GLOBAL_OFFSET_TABLE_`
    /// must exist.
    #[must_use]
    pub fn uses_got_base(self) -> bool {
        matches!(
            self.kind,
            Kind::GotSlotRel | Kind::GotRel | Kind::GotBasePc | Kind::GotPageOff
        )
    }

    /// Whether the relocation is an initial-exec access through a GOT entry
    /// holding the thread pointer offset.
    #[must_use]
    pub fn needs_gottpoff(self) -> bool {
        matches!(self.kind, Kind::GdToIe | Kind::DescToIe)
            || (self.needs_got() && self.slot == GotKind::TpOff)
    }

    /// Whether the relocation is a TLS access.
    #[must_use]
    pub fn is_tls(self) -> bool {
        matches!(
            self.kind,
            Kind::TpOff
                | Kind::DtpOff
                | Kind::GdToLe
                | Kind::LdToLe
                | Kind::IeToLe
                | Kind::DescToLe
                | Kind::DescCallToLe
                | Kind::GdToIe
                | Kind::DescToIe
        ) || (self.needs_got() && self.slot != GotKind::Address)
    }
}

/// How TLS accesses to one variable are linked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsMode {
    /// The variable is in the executable being linked: relax to local-exec.
    LocalExec,
    /// An executable accessing a shared library's variable: relax
    /// general-dynamic and descriptors to initial-exec.
    InitialExec,
    /// A shared object: keep the dynamic models.
    Dynamic,
}

/// What [`Arch::classify`] needs to know besides the relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassifyContext {
    /// GOT-indirect accesses may be rewritten into direct ones.
    pub relax_got: bool,
    /// The output is position-independent, so an address cannot become an
    /// immediate operand.
    pub pic: bool,
    /// How TLS accesses to the relocation's symbol are linked.
    pub tls: TlsMode,
    /// How local-dynamic accesses are linked: by the kind of output, not
    /// the variable.
    pub tls_ld: TlsMode,
    /// The relocated section holds code (`SHF_EXECINSTR`).
    pub code: bool,
}

impl ClassifyContext {
    /// The context of a static executable: everything relaxes.
    #[must_use]
    pub const fn static_exec(relax_got: bool) -> Self {
        Self {
            relax_got,
            pic: false,
            tls: TlsMode::LocalExec,
            tls_ld: TlsMode::LocalExec,
            code: false,
        }
    }
}

/// Why a relocation cannot be handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClassifyError {
    /// The type is unknown or not valid in a relocatable object.
    Unsupported,
    /// A TLS relaxation found an instruction it cannot rewrite.
    BadTlsInstruction,
}

/// Why a relocation could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The value does not fit the field.
    Overflow,
    /// The field lies outside the section.
    OutOfBounds,
    /// A relaxation found an instruction it cannot rewrite.
    BadInstruction,
}

/// The role of a dynamic relocation, without naming its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynKind {
    /// The load base is added to the addend.
    Relative,
    /// The load base is added and the result called to get the value.
    Irelative,
    /// A lazily bound PLT slot.
    JumpSlot,
    /// A GOT slot holding a symbol's address.
    GlobDat,
    /// A copy of a shared library's data.
    Copy,
    /// A 64-bit absolute address of a symbol.
    Abs64,
    /// The TLS module ID of a symbol's module.
    DtpMod,
    /// A symbol's offset in its module's TLS block.
    DtpOff,
    /// A symbol's offset from the thread pointer.
    TpOff,
    /// A TLS descriptor.
    TlsDesc,
}

/// The values a relaxation may need, all computed by the writer.
#[derive(Clone, Copy, Debug, Default)]
pub struct RelaxValues {
    /// `S + A - TP`, the thread pointer offset of the target.
    pub tpoff: i64,
    /// The address of the GOT entry the relaxed sequence reads, if it keeps
    /// one (initial-exec).
    pub got: u64,
    /// `G + A - P` for that entry.
    pub got_pc: i64,
    /// The place being relocated.
    pub place: u64,
    /// The GOT base, which i386 initial-exec code addresses the entry from.
    pub got_base: u64,
}

/// A direct branch, as range-extension thunk planning and the writer both
/// see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Branch {
    /// The relocation type.
    pub r_type: u32,
    /// The address of the branch instruction.
    pub place: u64,
    /// The symbol's address plus addend, or the stub's when the branch
    /// goes through one.
    pub target: u64,
    /// The callee's `st_other` (0 when unknown).
    pub st_other: u8,
    /// The branch goes to a PLT or IFUNC stub.
    pub via_stub: bool,
    /// The GOT word that stub jumps through, for calls that need a stub of
    /// their own (PowerPC64 code without a TOC pointer).
    pub slot: Option<u64>,
}

/// Options that change the shape of PLT entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PltFlags {
    /// The PLT header starts with a landing pad: x86-64 `endbr64`, or
    /// AArch64 `bti c` because PLT entries reach the header through
    /// `br x17`.
    pub landing_pad: bool,
    /// PLT entries start with one too. On x86-64 that is IBT (and the
    /// jumps move to `.plt.sec`); on AArch64 it is an executable's BTI PLT,
    /// whose entries may be a function's canonical address and so the
    /// target of an indirect call (GNU ld gives a shared object's entries
    /// none).
    pub entry_landing_pad: bool,
    /// AArch64 `-z pac-plt`: entries authenticate the address they loaded
    /// (`autia1716`) before branching to it.
    pub authenticate: bool,
    /// The output is position-independent: i386 PLT entries address the
    /// GOT through `%ebx` rather than absolutely.
    pub pic: bool,
}

impl Arch {
    /// The architecture of ELF machine `e_machine`, if qld links it.
    #[must_use]
    pub fn from_machine(e_machine: u16) -> Option<Self> {
        match e_machine {
            EM_X86_64 => Some(Self::X86_64),
            EM_AARCH64 => Some(Self::AArch64),
            EM_RISCV => Some(Self::RiscV64),
            EM_LOONGARCH => Some(Self::LoongArch64),
            EM_PPC64 => Some(Self::Ppc64),
            crate::elf::read::consts::EM_386 => Some(Self::I386),
            _ => None,
        }
    }

    /// The architecture of `target`, if qld links it.
    #[must_use]
    pub fn from_target(target: Target) -> Option<Self> {
        match target.arch {
            Architecture::X86_64 => Some(Self::X86_64),
            Architecture::Aarch64 => Some(Self::AArch64),
            // Only little-endian RISC-V: `elf64briscv` needs big-endian ELF.
            Architecture::Riscv64 if target.endian == crate::target::Endianness::Little => {
                Some(Self::RiscV64)
            }
            Architecture::LoongArch64 => Some(Self::LoongArch64),
            Architecture::X86 => Some(Self::I386),
            Architecture::X86_64X32 => Some(Self::X32),
            Architecture::PowerPc64 if target.endian == crate::target::Endianness::Little => {
                Some(Self::Ppc64)
            }
            _ => None,
        }
    }

    /// The architecture of a link: the emulation (`-m`) when one was given,
    /// otherwise the first relocatable object's machine, otherwise the
    /// default target's ([`crate::elf::target::default_target`]).
    #[must_use]
    pub fn of<F: crate::elf::read::ElfFormat>(
        options: &LinkOptions,
        files: &[super::inputs::ElfInput<'_, F>],
    ) -> Self {
        options
            .target
            .and_then(Self::from_target)
            .or_else(|| Self::of_files(files))
            .or_else(|| Self::from_target(crate::elf::target::default_target()))
            .unwrap_or_default()
    }

    /// The architecture of the first relocatable object among `files`.
    #[must_use]
    pub fn of_files<F: crate::elf::read::ElfFormat>(
        files: &[super::inputs::ElfInput<'_, F>],
    ) -> Option<Self> {
        files.iter().find_map(|file| {
            let object = file.object.as_ref()?;
            match Self::from_machine(object.elf.elf().header().e_machine)? {
                // An ELF32 x86-64 object is x32.
                Self::X86_64 if F::WORD_SIZE == 4 => Some(Self::X32),
                arch => Some(arch),
            }
        })
    }

    /// Why object `file` cannot be linked with the others, if it cannot
    /// (RISC-V: a floating-point ABI other than the first object's).
    #[must_use]
    pub fn incompatible<F: crate::elf::read::ElfFormat>(
        self,
        files: &[super::inputs::ElfInput<'_, F>],
        file: usize,
    ) -> Option<String> {
        if self != Self::RiscV64 {
            return None;
        }
        // RISC-V objects only: not `-b binary` inputs, which are `EM_NONE`.
        let flags = |f: &super::inputs::ElfInput<'_, F>| {
            let header = f.object.as_ref()?.elf.elf().header();
            (header.e_machine == crate::elf::read::consts::EM_RISCV).then_some(header.e_flags)
        };
        let (first_index, first) = files
            .iter()
            .enumerate()
            .find_map(|(i, f)| Some((i, flags(f)?)))?;
        let this = files.get(file)?;
        let what = riscv::incompatible_flags(first, flags(this)?)?;
        let first_name = files
            .get(first_index)
            .map(|f| f.display())
            .unwrap_or_default();
        Some(format!(
            "{}: cannot link object files with different {what} from {first_name}",
            this.display()
        ))
    }

    /// The ELF class and byte order of the output.
    ///
    /// Every encoder and decoder in the ELF backend is generic over the
    /// matching [`ElfFormat`](crate::elf::read::ElfFormat), so the choice is
    /// made once per link and the loops that follow never branch on it.
    #[must_use]
    pub fn kind(self) -> crate::elf::read::ElfKind {
        match self {
            Self::I386 | Self::X32 => crate::elf::read::ElfKind::Elf32Le,
            Self::X86_64 | Self::AArch64 | Self::RiscV64 | Self::LoongArch64 | Self::Ppc64 => {
                crate::elf::read::ElfKind::Elf64Le
            }
        }
    }

    /// The addend of `SHT_REL` relocation `r_type` at `offset` in section
    /// `data`: the contents of the field it patches (i386). Architectures
    /// that use `SHT_RELA` never store one there.
    #[must_use]
    #[inline]
    pub fn implicit_addend(self, r_type: u32, data: &[u8], offset: u64) -> i64 {
        match self {
            Self::I386 => i386::implicit_addend(r_type, data, offset),
            _ => 0,
        }
    }

    /// Whether dynamic relocations are `SHT_REL`, their addends stored in
    /// the words they relocate (i386), rather than `SHT_RELA`.
    #[must_use]
    pub fn uses_rel(self) -> bool {
        self == Self::I386
    }

    /// Whether a field of `width` holds a whole address, so a dynamic
    /// relocation can fill it: 64 bits in ELF64, 32 in ELF32.
    #[must_use]
    #[inline]
    pub fn is_word(self, width: Width) -> bool {
        // The 64-bit architectures first, so that they pay for no other.
        if self != Self::I386 && self != Self::X32 {
            return width == Width::W64;
        }
        match self {
            Self::I386 => matches!(width, Width::Any32 | Width::U32 | Width::I32),
            // `R_X86_64_32`, GNU ld's pointer relocation for x32.
            _ => width == Width::U32,
        }
    }

    /// Size of one GOT or `.got.plt` entry: the class's word, except on
    /// x32, whose GOT entries are 8 bytes as on x86-64 (indirect calls and
    /// jumps read 64 bits, and TLS entries hold 64-bit offsets).
    #[must_use]
    #[inline]
    pub fn got_entry_size(self) -> u64 {
        if self == Self::X32 {
            8
        } else {
            self.kind().word_size()
        }
    }

    /// Size of one dynamic relocation entry.
    #[must_use]
    pub fn dyn_reloc_size(self) -> u64 {
        if self.uses_rel() {
            self.kind().rel_size()
        } else {
            self.kind().rela_size()
        }
    }

    /// The `e_machine` of the output.
    #[must_use]
    pub fn machine(self) -> u16 {
        match self {
            Self::I386 => crate::elf::read::consts::EM_386,
            Self::X86_64 | Self::X32 => EM_X86_64,
            Self::AArch64 => EM_AARCH64,
            Self::RiscV64 => EM_RISCV,
            Self::LoongArch64 => EM_LOONGARCH,
            Self::Ppc64 => EM_PPC64,
        }
    }

    /// The GNU ld emulation name, for diagnostics.
    #[must_use]
    pub fn emulation(self) -> &'static str {
        match self {
            Self::I386 => "elf_i386",
            Self::X86_64 => "elf_x86_64",
            Self::X32 => "elf32_x86_64",
            Self::AArch64 => "aarch64linux",
            Self::RiscV64 => "elf64lriscv",
            Self::LoongArch64 => "elf64loongarch",
            Self::Ppc64 => "elf64lppc",
        }
    }

    /// The name of relocation type `r_type`, as `readelf` prints it.
    #[must_use]
    pub fn reloc_name(self, r_type: u32) -> Option<&'static str> {
        match self {
            Self::LoongArch64 => loongarch::reloc_name(r_type),
            _ => reloc_name(self.machine(), r_type),
        }
    }

    /// That name, or the number when the type is unknown.
    #[must_use]
    pub fn reloc_label(self, r_type: u32) -> String {
        let r_type = match self {
            Self::LoongArch64 => loongarch::base_type(r_type),
            Self::Ppc64 => ppc64::base_type(r_type),
            _ => r_type,
        };
        self.reloc_name(r_type)
            .map_or_else(|| r_type.to_string(), str::to_owned)
    }

    /// The default maximum page size: what segment alignment assumes when
    /// `-z max-page-size` is not given.
    #[must_use]
    pub fn default_max_page(self) -> u64 {
        match self {
            Self::I386 => 0x1000,
            Self::X86_64 | Self::X32 | Self::RiscV64 => 0x1000,
            Self::AArch64 | Self::Ppc64 => 0x1_0000,
            // GNU ld's; lld assumes 64 KiB.
            Self::LoongArch64 => 0x4000,
        }
    }

    /// The address a non-PIE executable is linked at by default (GNU ld's
    /// text start, headers included).
    #[must_use]
    pub fn default_base(self) -> u64 {
        match self {
            Self::I386 => 0x0804_8000,
            Self::X86_64 | Self::X32 | Self::AArch64 => super::layout::DEFAULT_BASE,
            Self::RiscV64 => 0x1_0000,
            Self::LoongArch64 => 0x1_2000_0000,
            // The first 256 MiB segment boundary.
            Self::Ppc64 => 0x1000_0000,
        }
    }

    /// The number of words `.got.plt` reserves for the dynamic linker:
    /// `_DYNAMIC`, the link map and the resolver on x86-64 and AArch64; the
    /// resolver and the link map on RISC-V, LoongArch and PowerPC64.
    #[must_use]
    pub fn got_plt_reserved(self) -> u64 {
        match self {
            Self::I386 => 3,
            Self::X86_64 | Self::X32 | Self::AArch64 => 3,
            Self::RiscV64 | Self::LoongArch64 | Self::Ppc64 => 2,
        }
    }

    /// The `e_flags` of the output, from the input objects: the LoongArch
    /// ABI and object ABI version, or the PowerPC64 ABI version (2); zero
    /// elsewhere.
    #[must_use]
    pub fn output_flags<F: crate::elf::read::ElfFormat>(
        self,
        files: &[super::inputs::ElfInput<'_, F>],
    ) -> u32 {
        if self == Self::Ppc64 {
            return ppc64::ABI_VERSION;
        }
        if self == Self::RiscV64 {
            return riscv::output_flags(
                files
                    .iter()
                    .filter_map(|f| f.object.as_ref())
                    .map(|o| o.elf.elf().header().e_flags),
            );
        }
        if self != Self::LoongArch64 {
            return 0;
        }
        loongarch::output_flags(files.iter().filter_map(|file| {
            let object = file.object.as_ref()?;
            let header = object.elf.elf().header();
            if header.e_machine != EM_LOONGARCH {
                return None;
            }
            let code = object
                .sections
                .iter()
                .any(|section| section.header.sh_flags & SHF_EXECINSTR != 0);
            Some((header.e_flags, code))
        }))
    }

    /// Whether [`Arch::annotate`] changes anything on this architecture, so
    /// relocation loops must look one relocation ahead
    /// ([`for_each_relocation!`]).
    #[must_use]
    pub fn annotates(self) -> bool {
        matches!(self, Self::LoongArch64 | Self::Ppc64)
    }

    /// `rel` with what the architecture needs to know about the relocation
    /// that follows it (`next`) folded into its type: on LoongArch, whether
    /// an `R_LARCH_RELAX` allows the instructions to be relaxed
    /// ([`loongarch::RELAX_HINT`]); on PowerPC64, whether a
    /// `__tls_get_addr` marker is on a PC-relative call
    /// ([`ppc64::PCREL_CALL_HINT`]). Other architectures get `rel` back.
    #[must_use]
    pub fn annotate(self, rel: Relocation, next: Option<&Relocation>) -> Relocation {
        if self == Self::Ppc64 {
            return ppc64::annotate(rel, next);
        }
        if self == Self::LoongArch64
            && let Some(next) = next
            && next.r_type == loongarch::R_LARCH_RELAX
            && next.offset == rel.offset
        {
            return Relocation {
                r_type: rel.r_type | loongarch::RELAX_HINT,
                ..rel
            };
        }
        rel
    }

    /// The page delta a page-relative relocation `r_type` at `place`
    /// computes to `target`: `Page(target) - Page(place)`, adjusted on
    /// LoongArch for the sign of the low part that completes it.
    #[must_use]
    pub fn page_delta(self, target: u64, place: u64, r_type: u32) -> u64 {
        match self {
            Self::I386 => {
                crate::arch::aarch64::page(target).wrapping_sub(crate::arch::aarch64::page(place))
            }
            Self::LoongArch64 => loongarch::page_delta(target, place, r_type),
            Self::X86_64 | Self::X32 | Self::AArch64 | Self::RiscV64 | Self::Ppc64 => {
                let page = crate::arch::aarch64::page;
                page(target).wrapping_sub(page(place))
            }
        }
    }

    /// Rewrites an instruction pair classified [`Kind::Relax`], given the
    /// value `target` (`S + A`) and the `place` of the relocation.
    ///
    /// # Errors
    ///
    /// [`ApplyError`] when the rewrite does not apply.
    pub fn relax(
        self,
        out: &mut [u8],
        offset: u64,
        r_type: u32,
        target: u64,
        place: u64,
    ) -> Result<(), ApplyError> {
        match self {
            Self::I386 => Err(ApplyError::BadInstruction),
            Self::LoongArch64 => loongarch::relax(out, offset, r_type, target, place),
            Self::X86_64 | Self::X32 | Self::AArch64 | Self::RiscV64 | Self::Ppc64 => {
                Err(ApplyError::BadInstruction)
            }
        }
    }

    /// Whether layout shrinks code sections by linker relaxation, so it
    /// iterates through [`shrink::layout`].
    #[must_use]
    pub fn relaxes(self) -> bool {
        shrink::applies(self)
    }

    /// Whether `.got.plt[0]` holds the address of `_DYNAMIC`.
    #[must_use]
    pub fn got_plt_holds_dynamic(self) -> bool {
        self.got_plt_reserved() == 3
    }

    /// Rejects options whose AArch64 effect is not implemented, rather
    /// than silently producing a binary that does not have it.
    ///
    /// # Errors
    ///
    /// None at present.
    pub fn check_options(self, options: &LinkOptions) -> crate::error::Result<()> {
        let _ = options;
        Ok(())
    }

    /// Rejects options a linker script layout does not implement: it
    /// places no thunk pool, so it has nowhere to put Cortex-A53 erratum
    /// patches.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::Unimplemented`] for the Cortex-A53 erratum
    /// workarounds.
    pub fn check_script_options(self, options: &LinkOptions) -> crate::error::Result<()> {
        if self == Self::AArch64 && aarch64_errata::enabled(options) {
            return Err(crate::error::Error::Unimplemented(
                "--fix-cortex-a53-843419 and --fix-cortex-a53-835769 with a linker script".into(),
            ));
        }
        Ok(())
    }

    /// Whether calls can fall out of range, so layout has to insert
    /// range-extension thunks.
    #[must_use]
    pub fn needs_thunks(self) -> bool {
        matches!(self, Self::AArch64 | Self::Ppc64)
    }

    /// Number of bytes one range-extension thunk occupies.
    #[must_use]
    pub fn thunk_size(self) -> u64 {
        match self {
            Self::Ppc64 => crate::arch::ppc64::THUNK_SIZE,
            _ => crate::arch::aarch64::THUNK_SIZE,
        }
    }

    /// Writes the range-extension thunk at address `address` for thunk key
    /// `key` ([`Arch::branch_thunk`]) into `out` at `at`.
    ///
    /// # Errors
    ///
    /// [`ApplyError::Overflow`] when the target is out of the thunk's reach.
    pub fn write_thunk(
        self,
        out: &mut [u8],
        at: u64,
        address: u64,
        key: u64,
    ) -> Result<(), ApplyError> {
        match self {
            Self::Ppc64 => crate::arch::ppc64::write_thunk(out, at, address, key)
                .map_err(|_| ApplyError::Overflow),
            _ => crate::arch::aarch64::write_thunk(out, at, address, key)
                .map_err(|_| ApplyError::Overflow),
        }
    }

    /// Whether relocation `r_type` is a direct branch that range-extension
    /// thunks serve.
    #[must_use]
    pub fn is_thunk_branch(self, r_type: u32) -> bool {
        use crate::elf::read::consts::aarch64 as a64;
        match self {
            Self::I386 => false,
            Self::X86_64 | Self::X32 | Self::RiscV64 | Self::LoongArch64 => false,
            Self::AArch64 => matches!(r_type, a64::R_AARCH64_CALL26 | a64::R_AARCH64_JUMP26),
            Self::Ppc64 => ppc64::is_thunk_branch(r_type),
        }
    }

    /// The address a direct branch jumps to, before any thunk: PowerPC64
    /// enters a function of this output at its local entry point.
    #[must_use]
    pub fn branch_destination(self, branch: Branch) -> u64 {
        match self {
            Self::Ppc64 => ppc64::branch_destination(branch),
            _ => branch.target,
        }
    }

    /// The key of the range-extension thunk `branch` goes through, if it
    /// needs one: its destination (thunks are shared by key), on PowerPC64
    /// with the kind of thunk in the low bits.
    #[must_use]
    pub fn branch_thunk(self, branch: Branch) -> Option<u64> {
        match self {
            Self::I386 => None,
            Self::X86_64 | Self::X32 | Self::RiscV64 | Self::LoongArch64 => None,
            Self::AArch64 => (self.is_thunk_branch(branch.r_type)
                && !crate::arch::aarch64::branch_in_range(branch.place, branch.target))
            .then_some(branch.target),
            Self::Ppc64 => ppc64::branch_thunk(branch),
        }
    }

    /// Rewrites what a direct call at `offset` needs besides its
    /// displacement: on PowerPC64, the `nop` after a call through a stub
    /// becomes the reload of the TOC pointer.
    ///
    /// # Errors
    ///
    /// [`ApplyError::BadInstruction`] for calls the architecture cannot
    /// link.
    pub fn finish_call(
        self,
        out: &mut [u8],
        offset: u64,
        branch: Branch,
    ) -> Result<(), ApplyError> {
        match self {
            Self::Ppc64 => ppc64::finish_call(out, offset, branch),
            _ => Ok(()),
        }
    }

    /// Whether `-z separate-code` is the default, as it is for GNU ld's
    /// x86 targets but not for AArch64.
    #[must_use]
    pub fn separate_code_by_default(self) -> bool {
        matches!(self, Self::X86_64 | Self::X32 | Self::I386)
    }

    /// The default program interpreter on Linux.
    #[must_use]
    pub fn default_interpreter(self) -> &'static str {
        match self {
            Self::I386 => "/lib/ld-linux.so.2",
            Self::X86_64 => "/lib64/ld-linux-x86-64.so.2",
            Self::X32 => "/libx32/ld-linux-x32.so.2",
            Self::AArch64 => "/lib/ld-linux-aarch64.so.1",
            Self::RiscV64 => riscv::interpreter(riscv::EF_RISCV_FLOAT_ABI),
            Self::LoongArch64 => "/lib64/ld-linux-loongarch-lp64d.so.1",
            Self::Ppc64 => "/lib64/ld64.so.2",
        }
    }

    /// Whether TLS uses variant I (the thread pointer is below the block,
    /// after a two-word thread control block) rather than variant II.
    #[must_use]
    pub fn tls_variant1(self) -> bool {
        matches!(
            self,
            Self::AArch64 | Self::RiscV64 | Self::LoongArch64 | Self::Ppc64
        )
    }

    /// The size of the thread control block variant I reserves below the
    /// TLS block.
    #[must_use]
    pub fn tcb_size(self) -> u64 {
        match self {
            Self::I386 => 0,
            Self::X86_64 | Self::X32 | Self::RiscV64 | Self::LoongArch64 | Self::Ppc64 => 0,
            Self::AArch64 => 16,
        }
    }

    /// Where the thread pointer is relative to the start of the TLS block,
    /// when the ABI fixes it there (PowerPC64: 0x7000 bytes past it)
    /// rather than past a thread control block.
    #[must_use]
    pub fn tp_past_tls_start(self) -> Option<u64> {
        match self {
            Self::Ppc64 => Some(crate::arch::ppc64::TP_OFFSET),
            _ => None,
        }
    }

    /// How far past the start of a module's TLS block the dynamic thread
    /// vector points (PowerPC64: 0x8000, RISC-V: 0x800), which biases
    /// `@dtprel` values.
    #[must_use]
    pub fn dtv_offset(self) -> u64 {
        match self {
            Self::Ppc64 => crate::arch::ppc64::DTV_OFFSET,
            Self::RiscV64 => crate::arch::riscv::DTP_OFFSET,
            _ => 0,
        }
    }

    /// Where the GOT base that GOT-relative relocations use is, from the
    /// start of `.got`, when it is not `.got.plt` (PowerPC64: the TOC
    /// pointer, `.got + 0x8000`).
    #[must_use]
    pub fn toc_bias(self) -> Option<u64> {
        match self {
            Self::Ppc64 => Some(crate::arch::ppc64::TOC_BIAS),
            _ => None,
        }
    }

    /// Words reserved at the start of `.got` (PowerPC64 keeps the TOC
    /// pointer's link-time value there).
    #[must_use]
    pub fn got_header_words(self) -> u64 {
        match self {
            Self::Ppc64 => 1,
            _ => 0,
        }
    }

    /// The `DT_PPC64_GLINK` value, as an offset from the start of `.plt`:
    /// 32 bytes before the first lazy entry, where glibc expects it.
    #[must_use]
    pub fn glink_offset(self) -> Option<u64> {
        match self {
            Self::Ppc64 => Some(crate::arch::ppc64::GLINK_HEADER_SIZE.wrapping_sub(32)),
            _ => None,
        }
    }

    /// Whether a dynamic output calls PLT entries through `.plt.sec`:
    /// x86-64 with IBT, and PowerPC64 always, its call stubs being there
    /// while `.plt` holds the lazy-binding entries.
    #[must_use]
    pub fn has_plt_sec(self, ibt: bool) -> bool {
        match self {
            Self::I386 => ibt,
            Self::X86_64 | Self::X32 => ibt,
            Self::AArch64 | Self::RiscV64 | Self::LoongArch64 => false,
            Self::Ppc64 => true,
        }
    }

    /// Size of one `.plt.sec` entry.
    #[must_use]
    pub fn plt_sec_entry_size(self, flags: PltFlags) -> u64 {
        match self {
            Self::Ppc64 => crate::arch::ppc64::PLT_CALL_STUB_SIZE,
            _ => self.plt_entry_size(flags),
        }
    }

    /// Whether a preemptible function that also has a GOT entry is called
    /// through `.plt.got` (jumping through that entry) rather than getting
    /// a PLT slot. PowerPC64 linkers give every called function a slot, and
    /// so does GNU ld's AArch64 backend, which has no `.plt.got`.
    #[must_use]
    pub fn uses_plt_got(self) -> bool {
        !matches!(self, Self::Ppc64 | Self::AArch64)
    }

    /// Whether a dynamic output's `IRELATIVE` relocations go to
    /// `.rela.plt`, with the lazy PLT relocations. PowerPC64's dynamic
    /// linker sets the lazy slots up itself and never applies `.rela.plt`
    /// entries one by one, so GNU ld puts them in `.rela.dyn` there.
    #[must_use]
    pub fn irelative_in_rela_plt(self) -> bool {
        self != Self::Ppc64
    }

    /// The number written for dynamic relocation `kind`.
    #[must_use]
    pub fn dyn_reloc(self, kind: DynKind) -> u32 {
        use crate::elf::read::consts::{aarch64 as a64, ppc64 as p64, riscv as rv, x86_64 as x64};
        match self {
            Self::I386 => {
                use crate::elf::read::consts::i386 as x86;
                match kind {
                    DynKind::Relative => x86::R_386_RELATIVE,
                    DynKind::Irelative => x86::R_386_IRELATIVE,
                    DynKind::JumpSlot => x86::R_386_JUMP_SLOT,
                    DynKind::GlobDat => x86::R_386_GLOB_DAT,
                    DynKind::Copy => x86::R_386_COPY,
                    DynKind::Abs64 => x86::R_386_32,
                    DynKind::DtpMod => x86::R_386_TLS_DTPMOD32,
                    DynKind::DtpOff => x86::R_386_TLS_DTPOFF32,
                    DynKind::TpOff => x86::R_386_TLS_TPOFF,
                    DynKind::TlsDesc => x86::R_386_TLS_DESC,
                }
            }
            Self::X86_64 | Self::X32 => match kind {
                DynKind::Relative => x64::R_X86_64_RELATIVE,
                DynKind::Irelative => x64::R_X86_64_IRELATIVE,
                DynKind::JumpSlot => x64::R_X86_64_JUMP_SLOT,
                DynKind::GlobDat => x64::R_X86_64_GLOB_DAT,
                DynKind::Copy => x64::R_X86_64_COPY,
                // x32 pointers are 32 bits: `R_X86_64_32`, as GNU ld writes.
                DynKind::Abs64 if self == Self::X32 => x64::R_X86_64_32,
                DynKind::Abs64 => x64::R_X86_64_64,
                DynKind::DtpMod => x64::R_X86_64_DTPMOD64,
                DynKind::DtpOff => x64::R_X86_64_DTPOFF64,
                DynKind::TpOff => x64::R_X86_64_TPOFF64,
                DynKind::TlsDesc => x64::R_X86_64_TLSDESC,
            },
            Self::AArch64 => match kind {
                DynKind::Relative => a64::R_AARCH64_RELATIVE,
                DynKind::Irelative => a64::R_AARCH64_IRELATIVE,
                DynKind::JumpSlot => a64::R_AARCH64_JUMP_SLOT,
                DynKind::GlobDat => a64::R_AARCH64_GLOB_DAT,
                DynKind::Copy => a64::R_AARCH64_COPY,
                DynKind::Abs64 => a64::R_AARCH64_ABS64,
                DynKind::DtpMod => a64::R_AARCH64_TLS_DTPMOD64,
                DynKind::DtpOff => a64::R_AARCH64_TLS_DTPREL64,
                DynKind::TpOff => a64::R_AARCH64_TLS_TPREL64,
                DynKind::TlsDesc => a64::R_AARCH64_TLSDESC,
            },
            Self::RiscV64 => match kind {
                DynKind::Relative => rv::R_RISCV_RELATIVE,
                DynKind::Irelative => rv::R_RISCV_IRELATIVE,
                DynKind::JumpSlot => rv::R_RISCV_JUMP_SLOT,
                DynKind::GlobDat | DynKind::Abs64 => rv::R_RISCV_64,
                DynKind::Copy => rv::R_RISCV_COPY,
                DynKind::DtpMod => rv::R_RISCV_TLS_DTPMOD64,
                DynKind::DtpOff => rv::R_RISCV_TLS_DTPREL64,
                DynKind::TpOff => rv::R_RISCV_TLS_TPREL64,
                DynKind::TlsDesc => rv::R_RISCV_TLSDESC,
            },
            // LoongArch has no GLOB_DAT: a GOT slot takes the symbolic
            // 64-bit relocation.
            Self::LoongArch64 => match kind {
                DynKind::Relative => loongarch::R_LARCH_RELATIVE,
                DynKind::Irelative => loongarch::R_LARCH_IRELATIVE,
                DynKind::JumpSlot => loongarch::R_LARCH_JUMP_SLOT,
                DynKind::GlobDat | DynKind::Abs64 => loongarch::R_LARCH_64,
                DynKind::Copy => loongarch::R_LARCH_COPY,
                DynKind::DtpMod => loongarch::R_LARCH_TLS_DTPMOD64,
                DynKind::DtpOff => loongarch::R_LARCH_TLS_DTPREL64,
                DynKind::TpOff => loongarch::R_LARCH_TLS_TPREL64,
                DynKind::TlsDesc => loongarch::R_LARCH_TLS_DESC64,
            },
            Self::Ppc64 => match kind {
                DynKind::Relative => p64::R_PPC64_RELATIVE,
                DynKind::Irelative => p64::R_PPC64_IRELATIVE,
                DynKind::JumpSlot => p64::R_PPC64_JMP_SLOT,
                DynKind::GlobDat => p64::R_PPC64_GLOB_DAT,
                DynKind::Copy => p64::R_PPC64_COPY,
                DynKind::Abs64 => p64::R_PPC64_ADDR64,
                DynKind::DtpMod => p64::R_PPC64_DTPMOD64,
                DynKind::DtpOff => p64::R_PPC64_DTPREL64,
                DynKind::TpOff => p64::R_PPC64_TPREL64,
                // PowerPC64 has no TLS descriptors.
                DynKind::TlsDesc => p64::R_PPC64_NONE,
            },
        }
    }

    /// Whether relocation `r_type` is a call or jump that goes through the
    /// PLT when its symbol is preemptible, and does not take the symbol's
    /// address (so `--icf=safe` may fold its target).
    #[must_use]
    #[inline(always)]
    pub fn is_branch(self, r_type: u32) -> bool {
        use crate::elf::read::consts::{aarch64 as a64, ppc64 as p64, x86_64 as x64};
        // x86-64 first, so that it pays for no other architecture.
        if self == Self::X86_64 {
            return matches!(r_type, x64::R_X86_64_PLT32 | x64::R_X86_64_PLT32_BND);
        }
        match self {
            Self::I386 => r_type == crate::elf::read::consts::i386::R_386_PLT32,
            Self::X86_64 => false, // answered above
            Self::X32 => matches!(r_type, x64::R_X86_64_PLT32 | x64::R_X86_64_PLT32_BND),
            Self::Ppc64 => matches!(r_type, p64::R_PPC64_REL24 | p64::R_PPC64_REL24_NOTOC),
            Self::AArch64 => matches!(
                r_type,
                a64::R_AARCH64_CALL26 | a64::R_AARCH64_JUMP26 | a64::R_AARCH64_PLT32
            ),
            Self::RiscV64 => riscv::is_branch(r_type),
            Self::LoongArch64 => loongarch::is_branch(r_type),
        }
    }

    /// Classifies relocation `r_type` at `offset` in section `data`.
    ///
    /// # Errors
    ///
    /// [`ClassifyError`] for unsupported types and unrecognized TLS code.
    // Only a dispatch: each architecture's classifier stays out of line.
    #[inline(always)]
    pub fn classify(
        self,
        r_type: u32,
        addend: i64,
        data: &[u8],
        offset: u64,
        context: ClassifyContext,
    ) -> Result<Class, ClassifyError> {
        match self {
            Self::I386 => i386::classify(r_type, addend, data, offset, context),
            Self::X86_64 => x86_64::classify(r_type, addend, data, offset, context),
            Self::X32 => x86_64::classify_x32(r_type, addend, data, offset, context),
            Self::AArch64 => aarch64::classify(r_type, context),
            Self::RiscV64 => riscv::classify(r_type, context),
            Self::LoongArch64 => loongarch::classify(r_type, addend, data, offset, context),
            Self::Ppc64 => ppc64::classify(r_type, data, offset, context),
        }
    }

    /// Rewrites a relaxed GOT-indirect instruction (x86-64 only).
    ///
    /// # Errors
    ///
    /// [`ApplyError`] when the instruction is not one that was classified as
    /// relaxable.
    pub fn relax_got(
        self,
        out: &mut [u8],
        offset: u64,
        kind: Kind,
        value: i64,
    ) -> Result<(), ApplyError> {
        match self {
            Self::I386 => i386::relax_got(out, offset, kind, value),
            Self::X86_64 => x86_64::relax_got(out, offset, kind, value),
            Self::X32 => x86_64::relax_got_x32(out, offset, kind, value),
            Self::AArch64 | Self::RiscV64 | Self::LoongArch64 | Self::Ppc64 => {
                Err(ApplyError::BadInstruction)
            }
        }
    }

    /// Rewrites a relaxed TLS access.
    ///
    /// # Errors
    ///
    /// [`ApplyError`] for unrecognized instruction sequences.
    pub fn relax_tls(
        self,
        out: &mut [u8],
        offset: u64,
        kind: Kind,
        r_type: u32,
        values: RelaxValues,
    ) -> Result<(), ApplyError> {
        match self {
            Self::I386 => i386::relax_tls(out, offset, kind, r_type, values),
            Self::X86_64 => x86_64::relax_tls(out, offset, kind, values),
            Self::X32 => x86_64::relax_tls_x32(out, offset, kind, values),
            Self::AArch64 => aarch64::relax_tls(out, offset, kind, r_type, values),
            // RISC-V sections are written by `riscv::apply`.
            Self::RiscV64 => Err(ApplyError::BadInstruction),
            Self::LoongArch64 => loongarch::relax_tls(out, offset, kind, r_type, values),
            Self::Ppc64 => ppc64::relax_tls(out, offset, kind, r_type, values),
        }
    }

    /// Size of the PLT header (`.plt` starts with it in a dynamic output).
    #[must_use]
    pub fn plt_header_size(self, flags: PltFlags) -> u64 {
        match self {
            Self::I386 => 16,
            Self::X86_64 | Self::X32 => 16,
            Self::AArch64 => {
                let _ = flags;
                32
            }
            Self::RiscV64 => riscv::PLT_HEADER_SIZE,
            Self::LoongArch64 => loongarch::PLT_HEADER_SIZE,
            Self::Ppc64 => crate::arch::ppc64::GLINK_HEADER_SIZE,
        }
    }

    /// Size of one `.plt` entry.
    #[must_use]
    pub fn plt_entry_size(self, flags: PltFlags) -> u64 {
        match self {
            Self::I386 => i386::PLT_ENTRY_SIZE,
            Self::X86_64 | Self::X32 => 16,
            Self::AArch64 => aarch64::plt_entry_size(flags),
            Self::RiscV64 => riscv::PLT_ENTRY_SIZE,
            Self::LoongArch64 => loongarch::PLT_ENTRY_SIZE,
            Self::Ppc64 => 4,
        }
    }

    /// Size of one `.plt.got` entry.
    #[must_use]
    pub fn plt_got_entry_size(self, flags: PltFlags) -> u64 {
        match self {
            Self::I386 => {
                if flags.landing_pad {
                    i386::PLT_ENTRY_SIZE
                } else {
                    i386::PLT_GOT_ENTRY_SIZE
                }
            }
            Self::X86_64 | Self::X32 if flags.landing_pad => 16,
            Self::X86_64 | Self::X32 => 8,
            Self::AArch64 => aarch64::plt_entry_size(aarch64::plt_got_flags(flags)),
            Self::RiscV64 => riscv::PLT_ENTRY_SIZE,
            Self::LoongArch64 => loongarch::PLT_ENTRY_SIZE,
            Self::Ppc64 => crate::arch::ppc64::PLT_CALL_STUB_SIZE,
        }
    }

    /// Alignment of `.plt`, `.plt.sec` and `.plt.got`.
    #[must_use]
    pub fn plt_align(self) -> u64 {
        16
    }

    /// Alignment of a static executable's `.plt`, which holds only IFUNC
    /// stubs: GNU ld's i386 stubs are 8 bytes and aligned to 8.
    #[must_use]
    pub fn iplt_align(self) -> u64 {
        if self == Self::I386 { 8 } else { 16 }
    }

    /// Size of an IFUNC stub in a static executable.
    #[must_use]
    pub fn iplt_entry_size(self, flags: PltFlags) -> u64 {
        match self {
            Self::I386 => i386::IPLT_ENTRY_SIZE,
            Self::X86_64 | Self::X32 => 16,
            Self::AArch64 => aarch64::plt_entry_size(flags),
            Self::RiscV64 => riscv::PLT_ENTRY_SIZE,
            Self::LoongArch64 => loongarch::PLT_ENTRY_SIZE,
            Self::Ppc64 => crate::arch::ppc64::PLT_CALL_STUB_SIZE,
        }
    }

    /// The value a lazy `.got.plt` slot holds before the dynamic linker
    /// binds it: the code that pushes the relocation index and jumps to the
    /// resolver.
    #[must_use]
    pub fn lazy_slot_value(self, plt: u64, entry: u64, flags: PltFlags) -> u64 {
        match self {
            Self::I386 => {
                // The `push` after the jump, or the whole entry with IBT.
                if flags.landing_pad {
                    entry
                } else {
                    entry.wrapping_add(6)
                }
            }
            // The `push` after the jump, or the whole entry with IBT.
            Self::X86_64 | Self::X32 if flags.landing_pad => entry,
            Self::X86_64 | Self::X32 => entry.wrapping_add(6),
            // The header pushes and jumps; entries do not. On LoongArch the
            // header finds the index from the entry's return address.
            Self::AArch64 | Self::RiscV64 | Self::LoongArch64 => plt,
            // The dynamic linker points every slot at its lazy entry.
            Self::Ppc64 => 0,
        }
    }

    /// Writes the PLT header at address `plt`, which jumps to the resolver
    /// through `.got.plt` (at `got_plt`).
    ///
    /// # Errors
    ///
    /// [`ApplyError`] when out of range.
    pub fn write_plt_header(
        self,
        out: &mut [u8],
        plt: u64,
        got_plt: u64,
        flags: PltFlags,
    ) -> Result<(), ApplyError> {
        match self {
            Self::I386 => i386::write_plt_header(out, got_plt, flags.pic),
            Self::X86_64 | Self::X32 => x86_64::write_plt_header(out, plt, got_plt),
            Self::AArch64 => aarch64::write_plt_header(out, plt, got_plt, flags),
            Self::RiscV64 => riscv::write_plt_header(out, plt, got_plt),
            Self::LoongArch64 => loongarch::write_plt_header(out, plt, got_plt),
            Self::Ppc64 => ppc64::write_plt_header(out, plt, got_plt),
        }
    }

    /// Writes `.plt` entry `index` at `entry`, which jumps through its
    /// `.got.plt` slot at `slot`.
    ///
    /// # Errors
    ///
    /// [`ApplyError`] when out of range.
    pub fn write_plt_entry(
        self,
        out: &mut [u8],
        entry: u64,
        slot: u64,
        index: u32,
        plt: u64,
        flags: PltFlags,
    ) -> Result<(), ApplyError> {
        match self {
            Self::I386 => {
                // `.got.plt` slot `index` follows the three reserved words,
                // which start at the GOT base.
                let got = slot.wrapping_sub(u64::from(index).wrapping_add(3).wrapping_mul(4));
                i386::write_plt_entry(
                    out,
                    entry,
                    slot,
                    index,
                    plt,
                    got,
                    flags.pic,
                    flags.landing_pad,
                )
            }
            Self::X86_64 | Self::X32 => {
                x86_64::write_plt_entry(out, entry, slot, index, plt, flags.landing_pad)
            }
            Self::AArch64 => aarch64::write_plt_entry(out, entry, slot, flags),
            Self::RiscV64 => riscv::write_plt_entry(out, entry, slot),
            Self::LoongArch64 => loongarch::write_plt_entry(out, entry, slot),
            Self::Ppc64 => ppc64::write_plt_entry(out, entry, plt),
        }
    }

    /// Writes a `.plt.sec` or `.plt.got` entry at `entry` that jumps through
    /// the GOT word at `slot`. `got_base` is the GOT base
    /// ([`crate::elf::values::Addresses::got_base`]), from which PowerPC64
    /// stubs address the slot.
    ///
    /// # Errors
    ///
    /// [`ApplyError`] when out of range.
    pub fn write_plt_jump(
        self,
        out: &mut [u8],
        entry: u64,
        slot: u64,
        flags: PltFlags,
        got_base: u64,
    ) -> Result<(), ApplyError> {
        match self {
            Self::I386 => i386::write_plt_jump(out, slot, got_base, flags.pic, flags.landing_pad),
            Self::X86_64 | Self::X32 => x86_64::write_plt_jump(out, entry, slot, flags.landing_pad),
            Self::AArch64 => {
                aarch64::write_plt_entry(out, entry, slot, aarch64::plt_got_flags(flags))
            }
            Self::RiscV64 => riscv::write_plt_entry(out, entry, slot),
            Self::LoongArch64 => loongarch::write_plt_entry(out, entry, slot),
            Self::Ppc64 => ppc64::write_call_stub(out, slot, got_base),
        }
    }

    /// Writes an IFUNC stub at `stub` that jumps through the GOT slot at
    /// `slot_address`; `got_base` as for [`Arch::write_plt_jump`].
    ///
    /// # Errors
    ///
    /// [`ApplyError`] when out of range.
    pub fn write_iplt(
        self,
        out: &mut [u8],
        stub: u64,
        slot_address: u64,
        flags: PltFlags,
        got_base: u64,
    ) -> Result<(), ApplyError> {
        match self {
            Self::I386 => i386::write_iplt(out, slot_address, got_base, flags.pic),
            Self::X86_64 | Self::X32 => x86_64::write_iplt(out, stub, slot_address),
            // GNU ld writes IFUNC stubs as ordinary PLT entries, landing
            // pad and authentication included.
            Self::AArch64 => aarch64::write_plt_entry(out, stub, slot_address, flags),
            Self::RiscV64 => riscv::write_plt_entry(out, stub, slot_address),
            Self::LoongArch64 => loongarch::write_plt_entry(out, stub, slot_address),
            Self::Ppc64 => ppc64::write_call_stub(out, slot_address, got_base),
        }
    }

    /// Replaces a branch to an undefined weak symbol, which has no address
    /// to branch to, with the instruction GNU ld writes there (AArch64: a
    /// `nop`; LoongArch: a branch to itself, since address 0 is out of
    /// reach). Returns `false` when the architecture keeps the branch.
    ///
    /// # Errors
    ///
    /// [`ApplyError`] when the instruction is outside the section.
    pub fn nop_undefined_branch(
        self,
        out: &mut [u8],
        offset: u64,
        r_type: u32,
    ) -> Result<bool, ApplyError> {
        match self {
            Self::I386 => Ok(false),
            Self::X86_64 | Self::X32 | Self::RiscV64 => Ok(false),
            Self::LoongArch64 => loongarch::undefined_weak_branch(out, offset, r_type),
            Self::AArch64 => aarch64::nop_undefined_branch(out, offset, r_type),
            Self::Ppc64 => ppc64::nop_undefined_branch(out, offset, r_type),
        }
    }

    /// Fills `out` with no-op instructions, as the architecture's default
    /// fill does for gaps in executable sections.
    pub fn write_nops(self, out: &mut [u8]) {
        match self {
            Self::I386 => x86_64::write_nops(out),
            Self::X86_64 | Self::X32 => x86_64::write_nops(out),
            Self::AArch64 => aarch64::write_nops(out),
            Self::RiscV64 => riscv::write_nops(out),
            Self::LoongArch64 => loongarch::write_nops(out),
            Self::Ppc64 => ppc64::write_nops(out),
        }
    }
}

fn slot<const N: usize>(out: &mut [u8], at: u64) -> Result<&mut [u8; N], ApplyError> {
    let start = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    out.get_mut(start..)
        .and_then(|rest| rest.first_chunk_mut::<N>())
        .ok_or(ApplyError::OutOfBounds)
}

/// Writes `value` into the field of width `width` at `offset`, checking that
/// it fits.
///
/// # Errors
///
/// [`ApplyError::Overflow`] or [`ApplyError::OutOfBounds`].
pub fn write_value(
    out: &mut [u8],
    offset: u64,
    width: Width,
    value: u64,
) -> Result<(), ApplyError> {
    let signed = value as i64;
    match width {
        Width::None => {}
        Width::W64 => *slot::<8>(out, offset)? = value.to_le_bytes(),
        Width::U32 => {
            let v = u32::try_from(value).map_err(|_| ApplyError::Overflow)?;
            *slot::<4>(out, offset)? = v.to_le_bytes();
        }
        Width::I32 => {
            let v = i32::try_from(signed).map_err(|_| ApplyError::Overflow)?;
            *slot::<4>(out, offset)? = v.to_le_bytes();
        }
        Width::Any32 => {
            if i32::try_from(signed).is_err() && u32::try_from(value).is_err() {
                return Err(ApplyError::Overflow);
            }
            *slot::<4>(out, offset)? = (value as u32).to_le_bytes();
        }
        Width::Any16 => {
            if i16::try_from(signed).is_err() && u16::try_from(value).is_err() {
                return Err(ApplyError::Overflow);
            }
            *slot::<2>(out, offset)? = (value as u16).to_le_bytes();
        }
        Width::I16 => {
            let v = i16::try_from(signed).map_err(|_| ApplyError::Overflow)?;
            *slot::<2>(out, offset)? = v.to_le_bytes();
        }
        Width::Any8 => {
            if i8::try_from(signed).is_err() && u8::try_from(value).is_err() {
                return Err(ApplyError::Overflow);
            }
            *slot::<1>(out, offset)? = [value as u8];
        }
        Width::I8 => {
            let v = i8::try_from(signed).map_err(|_| ApplyError::Overflow)?;
            *slot::<1>(out, offset)? = [v as u8];
        }
        Width::RiscV(field) => riscv::write_field(out, offset, field, value)?,
        Width::Field(field) => {
            if field.bytes() == 2 {
                let word = slot::<2>(out, offset)?;
                let encoded = field
                    .encode(u32::from(u16::from_le_bytes(*word)), signed)
                    .map_err(|_| ApplyError::Overflow)?;
                *word = (encoded as u16).to_le_bytes();
            } else {
                let word = slot::<4>(out, offset)?;
                let encoded = field
                    .encode(u32::from_le_bytes(*word), signed)
                    .map_err(|_| ApplyError::Overflow)?;
                *word = encoded.to_le_bytes();
            }
        }
        Width::LoongArch(field) => loongarch::write_field(out, offset, field, value)?,
        Width::Ppc(field) => write_ppc64(out, offset, field, signed)?,
    }
    Ok(())
}

/// Writes PowerPC64 field `field` at `offset`. Out of line, so that
/// [`write_value`] stays small for the other architectures.
#[inline(never)]
fn write_ppc64(
    out: &mut [u8],
    offset: u64,
    field: crate::arch::ppc64::Field,
    value: i64,
) -> Result<(), ApplyError> {
    use crate::arch::ppc64::EncodeError;
    let error = |e: EncodeError| match e {
        EncodeError::Overflow => ApplyError::Overflow,
        EncodeError::BadInstruction => ApplyError::BadInstruction,
    };
    if field == crate::arch::ppc64::Field::PcrelOpt {
        // Rewrites two instructions, `value` bytes apart.
        return ppc64::relax_pcrel_opt(out, offset, value);
    }
    match field.bytes() {
        2 => {
            let half = slot::<2>(out, offset)?;
            *half = field
                .encode16(u16::from_le_bytes(*half), value)
                .map_err(error)?
                .to_le_bytes();
        }
        8 => {
            let words = slot::<8>(out, offset)?;
            let (prefix, suffix) = words.split_at_mut(4);
            let old = (u64::from(u32::from_le_bytes([
                prefix[0], prefix[1], prefix[2], prefix[3],
            ])) << 32)
                | u64::from(u32::from_le_bytes([
                    suffix[0], suffix[1], suffix[2], suffix[3],
                ]));
            let encoded = field.encode64(old, value).map_err(error)?;
            prefix.copy_from_slice(&((encoded >> 32) as u32).to_le_bytes());
            suffix.copy_from_slice(&(encoded as u32).to_le_bytes());
        }
        _ => {
            let word = slot::<4>(out, offset)?;
            *word = field
                .encode32(u32::from_le_bytes(*word), value)
                .map_err(error)?
                .to_le_bytes();
        }
    }
    Ok(())
}

/// Adds `delta` to the field of width `width` at `offset`, wrapping as the
/// label-difference relocations do ([`Kind::Add`]; [`Kind::Sub`] passes
/// the negated value).
///
/// # Errors
///
/// [`ApplyError::OutOfBounds`], or [`ApplyError::BadInstruction`] for a
/// width that is not data.
pub fn add_value(out: &mut [u8], offset: u64, width: Width, delta: u64) -> Result<(), ApplyError> {
    match width {
        Width::W64 => {
            let word = slot::<8>(out, offset)?;
            *word = u64::from_le_bytes(*word).wrapping_add(delta).to_le_bytes();
        }
        Width::U32 | Width::I32 | Width::Any32 => {
            let word = slot::<4>(out, offset)?;
            *word = u32::from_le_bytes(*word)
                .wrapping_add(delta as u32)
                .to_le_bytes();
        }
        Width::Any16 | Width::I16 => {
            let word = slot::<2>(out, offset)?;
            *word = u16::from_le_bytes(*word)
                .wrapping_add(delta as u16)
                .to_le_bytes();
        }
        Width::Any8 | Width::I8 => {
            let [byte] = slot::<1>(out, offset)?;
            *byte = byte.wrapping_add(delta as u8);
        }
        Width::LoongArch(field) => loongarch::add_field(out, offset, field, delta)?,
        Width::None | Width::Field(_) | Width::RiscV(_) | Width::Ppc(_) => {
            return Err(ApplyError::BadInstruction);
        }
    }
    Ok(())
}

/// Runs `$body` for every relocation of `$relocations` (the
/// [`Relocations`](crate::elf::read::Relocations) of one section, whose
/// contents are `$data`), with `$rel` bound to it, as a `for` loop does
/// (`continue` and `break` work).
///
/// `SHT_REL` relocations get the addend stored in the field they patch
/// ([`Arch::implicit_addend`]). On architectures that [`Arch::annotates`],
/// `$rel` is [`Arch::annotate`]d with the relocation that follows it. Each
/// choice is made once, and the body is expanded into one loop per case,
/// so an architecture pays per relocation only for what it uses.
macro_rules! for_each_relocation {
    ($arch:expr, $relocations:expr, $data:expr, |$rel:ident| $body:block) => {{
        let arch: $crate::elf::arch::Arch = $arch;
        match $relocations {
            $crate::elf::read::Relocations::Rela(relas) => {
                if arch.annotates() {
                    let mut relocations = relas.iter().peekable();
                    while let Some(current) = relocations.next() {
                        let $rel = arch.annotate(current, relocations.peek());
                        $body
                    }
                } else {
                    for $rel in relas.iter() $body
                }
            }
            $crate::elf::read::Relocations::Rel(rels) => {
                let data: &[u8] = $data;
                for current in rels.iter() {
                    let $rel = $crate::elf::read::Relocation {
                        addend: arch.implicit_addend(current.r_type, data, current.offset),
                        ..current
                    };
                    $body
                }
            }
        }
    }};
}
pub(crate) use for_each_relocation;

/// The byte width of a relocation field.
#[must_use]
pub fn width_bytes(width: Width) -> usize {
    match width {
        Width::None => 0,
        Width::W64 => 8,
        Width::U32 | Width::I32 | Width::Any32 => 4,
        Width::Any16 | Width::I16 => 2,
        Width::Any8 | Width::I8 => 1,
        Width::Field(field) => field.bytes(),
        Width::RiscV(field) => field.bytes(),
        Width::LoongArch(field) => field.bytes(),
        Width::Ppc(field) => field.bytes(),
    }
}
