//! Per-architecture relocation handling.
//!
//! Each architecture module classifies relocation types, rewrites relaxable
//! instruction sequences and writes linker-generated stubs. The rest of the
//! ELF backend never names a relocation constant: it asks [`Arch`], which is
//! chosen once per link ([`Arch::of`]) and dispatches to the module for
//! x86-64, AArch64 or LoongArch64.
//!
//! The vocabulary is shared, so the relocation scan and the writer run one
//! loop for every architecture:
//!
//! - [`Kind`] says what a relocation computes (`S + A`, `Page(S + A) -
//!   Page(P)`, a GOT slot's address, a thread pointer offset, …) and
//!   [`GotKind`] which GOT entry it goes through;
//! - [`Width`] says how the result is stored: a data field, or an AArch64
//!   or LoongArch instruction field ([`crate::arch::aarch64::Field`],
//!   [`crate::arch::loongarch::Field`]);
//! - [`DynKind`] names the dynamic relocation a GOT slot, a PLT slot or a
//!   copy needs, without naming its number.

pub mod aarch64;
pub mod aarch64_errata;
pub mod loongarch;
pub mod thunk;
pub mod x86_64;

use crate::args::LinkOptions;
use crate::elf::read::Relocation;
use crate::elf::read::consts::{EM_AARCH64, EM_LOONGARCH, EM_X86_64, SHF_EXECINSTR, reloc_name};
use crate::target::{Architecture, Target};

/// The architecture an ELF link targets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Arch {
    /// x86-64.
    #[default]
    X86_64,
    /// AArch64 (LP64, little-endian).
    AArch64,
    /// LoongArch64 (LP64D, little-endian).
    LoongArch64,
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
    /// x86-64: a relaxed `REX_GOTPCRELX` to an immediate operand, `S`.
    RelaxGotPcNoPic,
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
    /// A LoongArch instruction (or data) field.
    LoongArch(crate::arch::loongarch::Field),
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
}

impl Arch {
    /// The architecture of ELF machine `e_machine`, if qld links it.
    #[must_use]
    pub fn from_machine(e_machine: u16) -> Option<Self> {
        match e_machine {
            EM_X86_64 => Some(Self::X86_64),
            EM_AARCH64 => Some(Self::AArch64),
            EM_LOONGARCH => Some(Self::LoongArch64),
            _ => None,
        }
    }

    /// The architecture of `target`, if qld links it.
    #[must_use]
    pub fn from_target(target: Target) -> Option<Self> {
        match target.arch {
            Architecture::X86_64 => Some(Self::X86_64),
            Architecture::Aarch64 => Some(Self::AArch64),
            Architecture::LoongArch64 => Some(Self::LoongArch64),
            _ => None,
        }
    }

    /// The architecture of a link: the emulation (`-m`) when one was given,
    /// otherwise the first relocatable object's machine.
    #[must_use]
    pub fn of(options: &LinkOptions, files: &[super::inputs::ElfInput<'_>]) -> Self {
        options
            .target
            .and_then(Self::from_target)
            .or_else(|| Self::of_files(files))
            .unwrap_or_default()
    }

    /// The architecture of the first relocatable object among `files`.
    #[must_use]
    pub fn of_files(files: &[super::inputs::ElfInput<'_>]) -> Option<Self> {
        files.iter().find_map(|file| {
            let object = file.object.as_ref()?;
            Self::from_machine(object.elf.elf().header().e_machine)
        })
    }

    /// The `e_machine` of the output.
    #[must_use]
    pub fn machine(self) -> u16 {
        match self {
            Self::X86_64 => EM_X86_64,
            Self::AArch64 => EM_AARCH64,
            Self::LoongArch64 => EM_LOONGARCH,
        }
    }

    /// The GNU ld emulation name, for diagnostics.
    #[must_use]
    pub fn emulation(self) -> &'static str {
        match self {
            Self::X86_64 => "elf_x86_64",
            Self::AArch64 => "aarch64linux",
            Self::LoongArch64 => "elf64loongarch",
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
            Self::X86_64 => 0x1000,
            Self::AArch64 => 0x1_0000,
            // GNU ld's; lld assumes 64 KiB.
            Self::LoongArch64 => 0x4000,
        }
    }

    /// The address a non-PIE executable is linked at by default (GNU ld's
    /// text start, headers included).
    #[must_use]
    pub fn default_base(self) -> u64 {
        match self {
            Self::X86_64 | Self::AArch64 => super::layout::DEFAULT_BASE,
            Self::LoongArch64 => 0x1_2000_0000,
        }
    }

    /// The number of words `.got.plt` reserves for the dynamic linker:
    /// `_DYNAMIC`, the link map and the resolver on x86-64 and AArch64; the
    /// resolver and the link map on LoongArch.
    #[must_use]
    pub fn got_plt_reserved(self) -> u64 {
        match self {
            Self::X86_64 | Self::AArch64 => 3,
            Self::LoongArch64 => 2,
        }
    }

    /// The `e_flags` of the output, from the input objects: the LoongArch
    /// ABI and object ABI version; zero elsewhere.
    #[must_use]
    pub fn output_flags(self, files: &[super::inputs::ElfInput<'_>]) -> u32 {
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

    /// `rel` with what the architecture needs to know about the relocation
    /// that follows it (`next`) folded into its type: on LoongArch, whether
    /// an `R_LARCH_RELAX` allows the instructions to be relaxed
    /// ([`loongarch::RELAX_HINT`]). Other architectures get `rel` back.
    #[must_use]
    pub fn annotate(self, rel: Relocation, next: Option<&Relocation>) -> Relocation {
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
            Self::LoongArch64 => loongarch::page_delta(target, place, r_type),
            Self::X86_64 | Self::AArch64 => {
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
            Self::LoongArch64 => loongarch::relax(out, offset, r_type, target, place),
            Self::X86_64 | Self::AArch64 => Err(ApplyError::BadInstruction),
        }
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
        self == Self::AArch64
    }

    /// Whether `-z separate-code` is the default, as it is for GNU ld's
    /// x86 targets but not for AArch64.
    #[must_use]
    pub fn separate_code_by_default(self) -> bool {
        self == Self::X86_64
    }

    /// The default program interpreter on Linux.
    #[must_use]
    pub fn default_interpreter(self) -> &'static str {
        match self {
            Self::X86_64 => "/lib64/ld-linux-x86-64.so.2",
            Self::AArch64 => "/lib/ld-linux-aarch64.so.1",
            Self::LoongArch64 => "/lib64/ld-linux-loongarch-lp64d.so.1",
        }
    }

    /// Whether TLS uses variant I (the thread pointer is below the block,
    /// after a two-word thread control block) rather than variant II.
    #[must_use]
    pub fn tls_variant1(self) -> bool {
        matches!(self, Self::AArch64 | Self::LoongArch64)
    }

    /// The size of the thread control block variant I reserves below the
    /// TLS block.
    #[must_use]
    pub fn tcb_size(self) -> u64 {
        match self {
            Self::X86_64 | Self::LoongArch64 => 0,
            Self::AArch64 => 16,
        }
    }

    /// The number written for dynamic relocation `kind`.
    #[must_use]
    pub fn dyn_reloc(self, kind: DynKind) -> u32 {
        use crate::elf::read::consts::{aarch64 as a64, x86_64 as x64};
        match self {
            Self::X86_64 => match kind {
                DynKind::Relative => x64::R_X86_64_RELATIVE,
                DynKind::Irelative => x64::R_X86_64_IRELATIVE,
                DynKind::JumpSlot => x64::R_X86_64_JUMP_SLOT,
                DynKind::GlobDat => x64::R_X86_64_GLOB_DAT,
                DynKind::Copy => x64::R_X86_64_COPY,
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
        }
    }

    /// Whether relocation `r_type` is a call or jump that goes through the
    /// PLT when its symbol is preemptible, and does not take the symbol's
    /// address (so `--icf=safe` may fold its target).
    #[must_use]
    pub fn is_branch(self, r_type: u32) -> bool {
        use crate::elf::read::consts::{aarch64 as a64, x86_64 as x64};
        match self {
            Self::X86_64 => matches!(r_type, x64::R_X86_64_PLT32 | x64::R_X86_64_PLT32_BND),
            Self::AArch64 => matches!(
                r_type,
                a64::R_AARCH64_CALL26 | a64::R_AARCH64_JUMP26 | a64::R_AARCH64_PLT32
            ),
            Self::LoongArch64 => loongarch::is_branch(r_type),
        }
    }

    /// Classifies relocation `r_type` at `offset` in section `data`.
    ///
    /// # Errors
    ///
    /// [`ClassifyError`] for unsupported types and unrecognized TLS code.
    pub fn classify(
        self,
        r_type: u32,
        addend: i64,
        data: &[u8],
        offset: u64,
        context: ClassifyContext,
    ) -> Result<Class, ClassifyError> {
        match self {
            Self::X86_64 => x86_64::classify(r_type, addend, data, offset, context),
            Self::AArch64 => aarch64::classify(r_type, context),
            Self::LoongArch64 => loongarch::classify(r_type, addend, data, offset, context),
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
            Self::X86_64 => x86_64::relax_got(out, offset, kind, value),
            Self::AArch64 | Self::LoongArch64 => Err(ApplyError::BadInstruction),
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
            Self::X86_64 => x86_64::relax_tls(out, offset, kind, values),
            Self::AArch64 => aarch64::relax_tls(out, offset, kind, r_type, values),
            Self::LoongArch64 => loongarch::relax_tls(out, offset, kind, r_type, values),
        }
    }

    /// Size of the PLT header (`.plt` starts with it in a dynamic output).
    #[must_use]
    pub fn plt_header_size(self, flags: PltFlags) -> u64 {
        match self {
            Self::X86_64 => 16,
            Self::AArch64 => {
                let _ = flags;
                32
            }
            Self::LoongArch64 => loongarch::PLT_HEADER_SIZE,
        }
    }

    /// Size of one `.plt` entry.
    #[must_use]
    pub fn plt_entry_size(self, flags: PltFlags) -> u64 {
        match self {
            Self::X86_64 => 16,
            Self::AArch64 => aarch64::plt_entry_size(flags),
            Self::LoongArch64 => loongarch::PLT_ENTRY_SIZE,
        }
    }

    /// Size of one `.plt.got` entry.
    #[must_use]
    pub fn plt_got_entry_size(self, flags: PltFlags) -> u64 {
        match self {
            Self::X86_64 if flags.landing_pad => 16,
            Self::X86_64 => 8,
            Self::AArch64 => aarch64::plt_entry_size(aarch64::plt_got_flags(flags)),
            Self::LoongArch64 => loongarch::PLT_ENTRY_SIZE,
        }
    }

    /// Alignment of `.plt`, `.plt.sec` and `.plt.got`.
    #[must_use]
    pub fn plt_align(self) -> u64 {
        16
    }

    /// Size of an IFUNC stub in a static executable.
    #[must_use]
    pub fn iplt_entry_size(self, flags: PltFlags) -> u64 {
        match self {
            Self::X86_64 => 16,
            Self::AArch64 => aarch64::plt_entry_size(flags),
            Self::LoongArch64 => loongarch::PLT_ENTRY_SIZE,
        }
    }

    /// The value a lazy `.got.plt` slot holds before the dynamic linker
    /// binds it: the code that pushes the relocation index and jumps to the
    /// resolver.
    #[must_use]
    pub fn lazy_slot_value(self, plt: u64, entry: u64, flags: PltFlags) -> u64 {
        match self {
            // The `push` after the jump, or the whole entry with IBT.
            Self::X86_64 if flags.landing_pad => entry,
            Self::X86_64 => entry.wrapping_add(6),
            // The header pushes and jumps; entries do not. On LoongArch the
            // header finds the index from the entry's return address.
            Self::AArch64 | Self::LoongArch64 => plt,
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
            Self::X86_64 => x86_64::write_plt_header(out, plt, got_plt),
            Self::AArch64 => aarch64::write_plt_header(out, plt, got_plt, flags),
            Self::LoongArch64 => loongarch::write_plt_header(out, plt, got_plt),
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
            Self::X86_64 => {
                x86_64::write_plt_entry(out, entry, slot, index, plt, flags.landing_pad)
            }
            Self::AArch64 => aarch64::write_plt_entry(out, entry, slot, flags),
            Self::LoongArch64 => loongarch::write_plt_entry(out, entry, slot),
        }
    }

    /// Writes a `.plt.sec` or `.plt.got` entry at `entry` that jumps through
    /// the GOT word at `slot`.
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
    ) -> Result<(), ApplyError> {
        match self {
            Self::X86_64 => x86_64::write_plt_jump(out, entry, slot, flags.landing_pad),
            Self::AArch64 => {
                aarch64::write_plt_entry(out, entry, slot, aarch64::plt_got_flags(flags))
            }
            Self::LoongArch64 => loongarch::write_plt_entry(out, entry, slot),
        }
    }

    /// Writes an IFUNC stub at `stub` that jumps through the GOT slot at
    /// `slot_address`.
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
    ) -> Result<(), ApplyError> {
        match self {
            Self::X86_64 => x86_64::write_iplt(out, stub, slot_address),
            // GNU ld writes IFUNC stubs as ordinary PLT entries, landing
            // pad and authentication included.
            Self::AArch64 => aarch64::write_plt_entry(out, stub, slot_address, flags),
            Self::LoongArch64 => loongarch::write_plt_entry(out, stub, slot_address),
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
            Self::X86_64 => Ok(false),
            Self::LoongArch64 => loongarch::undefined_weak_branch(out, offset, r_type),
            Self::AArch64 => aarch64::nop_undefined_branch(out, offset, r_type),
        }
    }

    /// Fills `out` with no-op instructions, as the architecture's default
    /// fill does for gaps in executable sections.
    pub fn write_nops(self, out: &mut [u8]) {
        match self {
            Self::X86_64 => x86_64::write_nops(out),
            Self::AArch64 => aarch64::write_nops(out),
            Self::LoongArch64 => loongarch::write_nops(out),
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
        Width::None | Width::Field(_) => return Err(ApplyError::BadInstruction),
    }
    Ok(())
}

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
        Width::LoongArch(field) => field.bytes(),
    }
}
