//! 32-bit Arm (`armelf_linux_eabi`: ARMv7-A, little-endian) relocations.
//!
//! Arm objects use `SHT_REL`: a relocation's addend is in the field it
//! patches, which for most relocations is an instruction field
//! ([`implicit_addend`], [`crate::arch::arm::Field::decode`]); dynamic
//! relocations are `SHT_REL` too, as on i386.
//!
//! Code comes in two instruction sets, A32 ("Arm") and Thumb, which call
//! each other. A Thumb function's symbol has bit 0 of its value set, so `S`
//! of a relocation against it carries the Thumb bit that the ABI's `T`
//! stands for. Besides [`classify`], the backend has its own section
//! writer ([`apply`]), because of what Arm branches need:
//!
//! - **Interworking.** A `bl` to a function in the other state becomes a
//!   `blx` (and back), in both A32 and Thumb, as GNU ld and lld do for
//!   `R_ARM_CALL` and `R_ARM_THM_CALL` against `STT_FUNC` symbols and PLT
//!   entries (which are A32). A branch that cannot change state (`b`,
//!   `b.w`, `bl<cond>`) goes through an interworking thunk.
//! - **Range extension.** A32 branches reach ±32 MiB, Thumb-2 `bl`/`b.w`
//!   ±16 MiB and `b<cond>.w` ±1 MiB. Farther branches go through a thunk
//!   placed by the shared thunk framework ([`super::thunk`]); [`thunks`]
//!   decides which branches need one. A thunk is in its callers' state and
//!   loads the destination with `movw`/`movt` (position-independent in a
//!   PIE or shared object) and `bx`s to it.
//! - **Undefined weak** branch targets without a PLT entry become `nop`s,
//!   as in GNU ld.
//!
//! The exception index (`.ARM.exidx`, [`exidx`]) is rebuilt as one table
//! sorted by code address, with `EXIDX_CANTUNWIND` entries for code
//! without unwind information and a terminating sentinel, and
//! `.ARM.attributes` sections are merged into one ([`attributes`]), which
//! also gives the float ABI of `e_flags`.
//!
//! **TLS** follows lld: general-dynamic, local-dynamic, initial-exec and
//! local-exec accesses are linked as they are, without relaxation (the
//! code sequences are not fixed enough to rewrite); GOT pairs and entries
//! are link-time constants in an executable.
//!
//! **The PLT** is GNU ld's: a 20-byte header that jumps to the resolver
//! through `.got.plt[2]` and 12-byte A32 entries (`add ip, pc; add ip, ip;
//! ldr pc, [ip]!`). Thumb callers reach it with `blx`.

#![deny(clippy::arithmetic_side_effects)]

pub mod apply;
pub mod attributes;
pub mod exidx;
pub mod thunks;

use crate::arch::arm::{self as insn, Field};

use super::{ApplyError, Class, ClassifyContext, ClassifyError, GotKind, Kind, Width};

/// No relocation.
pub const R_ARM_NONE: u32 = 0;
/// A32 `b`/`bl` (deprecated form of `CALL`/`JUMP24`).
pub const R_ARM_PC24: u32 = 1;
/// `S + A | T`, 32 bits.
pub const R_ARM_ABS32: u32 = 2;
/// `(S + A | T) - P`, 32 bits.
pub const R_ARM_REL32: u32 = 3;
/// A32 `ldr` from a literal, `S + A - P`.
pub const R_ARM_LDR_PC_G0: u32 = 4;
/// `S + A`, 16 bits.
pub const R_ARM_ABS16: u32 = 5;
/// `S + A`, 8 bits.
pub const R_ARM_ABS8: u32 = 8;
/// Thumb `bl`/`blx`.
pub const R_ARM_THM_CALL: u32 = 10;
/// Dynamic: a TLS descriptor.
pub const R_ARM_TLS_DESC: u32 = 13;
/// Dynamic: the TLS module ID.
pub const R_ARM_TLS_DTPMOD32: u32 = 17;
/// Dynamic: the offset in the module's TLS block.
pub const R_ARM_TLS_DTPOFF32: u32 = 18;
/// Dynamic: the offset from the thread pointer.
pub const R_ARM_TLS_TPOFF32: u32 = 19;
/// Dynamic: copy relocation.
pub const R_ARM_COPY: u32 = 20;
/// Dynamic: a GOT entry.
pub const R_ARM_GLOB_DAT: u32 = 21;
/// Dynamic: a PLT slot.
pub const R_ARM_JUMP_SLOT: u32 = 22;
/// Dynamic: the load base plus the addend.
pub const R_ARM_RELATIVE: u32 = 23;
/// `S + A - GOT_ORG`.
pub const R_ARM_GOTOFF32: u32 = 24;
/// `GOT_ORG + A - P`.
pub const R_ARM_BASE_PREL: u32 = 25;
/// `GOT(S) + A - GOT_ORG`.
pub const R_ARM_GOT_BREL: u32 = 26;
/// A32 `bl` (deprecated).
pub const R_ARM_PLT32: u32 = 27;
/// A32 `bl`/`blx`.
pub const R_ARM_CALL: u32 = 28;
/// A32 `b`, `bl<cond>`.
pub const R_ARM_JUMP24: u32 = 29;
/// Thumb-2 `b.w`.
pub const R_ARM_THM_JUMP24: u32 = 30;
/// `GOT_ORG + A`.
pub const R_ARM_BASE_ABS: u32 = 31;
/// `R_ARM_ABS32` on Linux (`.init_array`), or `R_ARM_REL32` with
/// `--target1-rel`.
pub const R_ARM_TARGET1: u32 = 38;
/// Marks a `bx` for `--fix-v4bx`.
pub const R_ARM_V4BX: u32 = 40;
/// `R_ARM_GOT_PREL` on Linux (exception tables' type information).
pub const R_ARM_TARGET2: u32 = 41;
/// `(S + A | T) - P`, 31 bits (`.ARM.exidx`, `.ARM.extab`).
pub const R_ARM_PREL31: u32 = 42;
/// A32 `movw` of `S + A | T`.
pub const R_ARM_MOVW_ABS_NC: u32 = 43;
/// A32 `movt` of `S + A`.
pub const R_ARM_MOVT_ABS: u32 = 44;
/// A32 `movw` of `(S + A | T) - P`.
pub const R_ARM_MOVW_PREL_NC: u32 = 45;
/// A32 `movt` of `S + A - P`.
pub const R_ARM_MOVT_PREL: u32 = 46;
/// Thumb `movw` of `S + A | T`.
pub const R_ARM_THM_MOVW_ABS_NC: u32 = 47;
/// Thumb `movt` of `S + A`.
pub const R_ARM_THM_MOVT_ABS: u32 = 48;
/// Thumb `movw` of `(S + A | T) - P`.
pub const R_ARM_THM_MOVW_PREL_NC: u32 = 49;
/// Thumb `movt` of `S + A - P`.
pub const R_ARM_THM_MOVT_PREL: u32 = 50;
/// Thumb-2 `b<cond>.w`.
pub const R_ARM_THM_JUMP19: u32 = 51;
/// Thumb-2 `adr.w`.
pub const R_ARM_THM_ALU_PREL_11_0: u32 = 53;
/// Thumb-2 `ldr.w` from a literal.
pub const R_ARM_THM_PC12: u32 = 54;
/// `S + A`, 32 bits, not to be turned into an interworking address.
pub const R_ARM_ABS32_NOI: u32 = 55;
/// `S + A - P`, 32 bits, likewise.
pub const R_ARM_REL32_NOI: u32 = 56;
/// A32 `add`/`sub` from the PC, unchecked.
pub const R_ARM_ALU_PC_G0_NC: u32 = 57;
/// A32 `add`/`sub` from the PC.
pub const R_ARM_ALU_PC_G0: u32 = 58;
/// A TLS descriptor's GOT entry (GNU descriptors).
pub const R_ARM_TLS_GOTDESC: u32 = 90;
/// A32 call to a TLS descriptor's resolver.
pub const R_ARM_TLS_CALL: u32 = 91;
/// A32 TLS descriptor sequence marker.
pub const R_ARM_TLS_DESCSEQ: u32 = 92;
/// Thumb call to a TLS descriptor's resolver.
pub const R_ARM_THM_TLS_CALL: u32 = 93;
/// `GOT(S) + A`.
pub const R_ARM_GOT_ABS: u32 = 95;
/// `GOT(S) + A - P`.
pub const R_ARM_GOT_PREL: u32 = 96;
/// C++ virtual table garbage collection marker.
pub const R_ARM_GNU_VTENTRY: u32 = 100;
/// C++ virtual table garbage collection marker.
pub const R_ARM_GNU_VTINHERIT: u32 = 101;
/// Thumb `b`.
pub const R_ARM_THM_JUMP11: u32 = 102;
/// Thumb `b<cond>`.
pub const R_ARM_THM_JUMP8: u32 = 103;
/// `GOT_TLSGD(S) + A - P`: general-dynamic.
pub const R_ARM_TLS_GD32: u32 = 104;
/// `GOT_TLSLDM + A - P`: local-dynamic.
pub const R_ARM_TLS_LDM32: u32 = 105;
/// `S + A - TLS`: the offset in the module's block.
pub const R_ARM_TLS_LDO32: u32 = 106;
/// `GOT_TPOFF(S) + A - P`: initial-exec.
pub const R_ARM_TLS_IE32: u32 = 107;
/// `S + A - TP`: local-exec.
pub const R_ARM_TLS_LE32: u32 = 108;
/// Thumb TLS descriptor sequence marker (16-bit).
pub const R_ARM_THM_TLS_DESCSEQ16: u32 = 129;
/// Thumb TLS descriptor sequence marker (32-bit).
pub const R_ARM_THM_TLS_DESCSEQ32: u32 = 130;
/// Dynamic: an IFUNC resolved at startup.
pub const R_ARM_IRELATIVE: u32 = 160;

/// Section type of `.ARM.exidx`.
pub const SHT_ARM_EXIDX: u32 = 0x7000_0001;
/// Section type of `.ARM.attributes`.
pub const SHT_ARM_ATTRIBUTES: u32 = 0x7000_0003;
/// `p_type` of the segment that covers `.ARM.exidx`.
pub const PT_ARM_EXIDX: u32 = 0x7000_0001;
/// `e_flags`: EABI version 5.
pub const EF_ARM_EABI_VER5: u32 = 0x0500_0000;
/// `e_flags`: the EABI version field.
pub const EF_ARM_EABIMASK: u32 = 0xff00_0000;
/// `e_flags`: the base procedure call standard (floating-point arguments
/// in core registers).
pub const EF_ARM_ABI_FLOAT_SOFT: u32 = 0x200;
/// `e_flags`: floating-point arguments in VFP registers.
pub const EF_ARM_ABI_FLOAT_HARD: u32 = 0x400;

/// Size of the PLT header.
pub const PLT_HEADER_SIZE: u64 = 20;
/// Size of one PLT entry (and IFUNC stub).
pub const PLT_ENTRY_SIZE: u64 = 12;

/// The name of relocation type `r_type`, as `readelf` prints it.
#[must_use]
pub fn reloc_name(r_type: u32) -> Option<&'static str> {
    Some(match r_type {
        R_ARM_NONE => "R_ARM_NONE",
        R_ARM_PC24 => "R_ARM_PC24",
        R_ARM_ABS32 => "R_ARM_ABS32",
        R_ARM_REL32 => "R_ARM_REL32",
        R_ARM_LDR_PC_G0 => "R_ARM_LDR_PC_G0",
        R_ARM_ABS16 => "R_ARM_ABS16",
        6 => "R_ARM_ABS12",
        7 => "R_ARM_THM_ABS5",
        R_ARM_ABS8 => "R_ARM_ABS8",
        9 => "R_ARM_SBREL32",
        R_ARM_THM_CALL => "R_ARM_THM_CALL",
        11 => "R_ARM_THM_PC8",
        12 => "R_ARM_BREL_ADJ",
        R_ARM_TLS_DESC => "R_ARM_TLS_DESC",
        R_ARM_TLS_DTPMOD32 => "R_ARM_TLS_DTPMOD32",
        R_ARM_TLS_DTPOFF32 => "R_ARM_TLS_DTPOFF32",
        R_ARM_TLS_TPOFF32 => "R_ARM_TLS_TPOFF32",
        R_ARM_COPY => "R_ARM_COPY",
        R_ARM_GLOB_DAT => "R_ARM_GLOB_DAT",
        R_ARM_JUMP_SLOT => "R_ARM_JUMP_SLOT",
        R_ARM_RELATIVE => "R_ARM_RELATIVE",
        R_ARM_GOTOFF32 => "R_ARM_GOTOFF32",
        R_ARM_BASE_PREL => "R_ARM_BASE_PREL",
        R_ARM_GOT_BREL => "R_ARM_GOT_BREL",
        R_ARM_PLT32 => "R_ARM_PLT32",
        R_ARM_CALL => "R_ARM_CALL",
        R_ARM_JUMP24 => "R_ARM_JUMP24",
        R_ARM_THM_JUMP24 => "R_ARM_THM_JUMP24",
        R_ARM_BASE_ABS => "R_ARM_BASE_ABS",
        R_ARM_TARGET1 => "R_ARM_TARGET1",
        39 => "R_ARM_SBREL31",
        R_ARM_V4BX => "R_ARM_V4BX",
        R_ARM_TARGET2 => "R_ARM_TARGET2",
        R_ARM_PREL31 => "R_ARM_PREL31",
        R_ARM_MOVW_ABS_NC => "R_ARM_MOVW_ABS_NC",
        R_ARM_MOVT_ABS => "R_ARM_MOVT_ABS",
        R_ARM_MOVW_PREL_NC => "R_ARM_MOVW_PREL_NC",
        R_ARM_MOVT_PREL => "R_ARM_MOVT_PREL",
        R_ARM_THM_MOVW_ABS_NC => "R_ARM_THM_MOVW_ABS_NC",
        R_ARM_THM_MOVT_ABS => "R_ARM_THM_MOVT_ABS",
        R_ARM_THM_MOVW_PREL_NC => "R_ARM_THM_MOVW_PREL_NC",
        R_ARM_THM_MOVT_PREL => "R_ARM_THM_MOVT_PREL",
        R_ARM_THM_JUMP19 => "R_ARM_THM_JUMP19",
        52 => "R_ARM_THM_JUMP6",
        R_ARM_THM_ALU_PREL_11_0 => "R_ARM_THM_ALU_PREL_11_0",
        R_ARM_THM_PC12 => "R_ARM_THM_PC12",
        R_ARM_ABS32_NOI => "R_ARM_ABS32_NOI",
        R_ARM_REL32_NOI => "R_ARM_REL32_NOI",
        R_ARM_ALU_PC_G0_NC => "R_ARM_ALU_PC_G0_NC",
        R_ARM_ALU_PC_G0 => "R_ARM_ALU_PC_G0",
        59 => "R_ARM_ALU_PC_G1_NC",
        60 => "R_ARM_ALU_PC_G1",
        61 => "R_ARM_ALU_PC_G2",
        62 => "R_ARM_LDR_PC_G1",
        63 => "R_ARM_LDR_PC_G2",
        64 => "R_ARM_LDRS_PC_G0",
        65 => "R_ARM_LDRS_PC_G1",
        66 => "R_ARM_LDRS_PC_G2",
        67 => "R_ARM_LDC_PC_G0",
        68 => "R_ARM_LDC_PC_G1",
        69 => "R_ARM_LDC_PC_G2",
        R_ARM_TLS_GOTDESC => "R_ARM_TLS_GOTDESC",
        R_ARM_TLS_CALL => "R_ARM_TLS_CALL",
        R_ARM_TLS_DESCSEQ => "R_ARM_TLS_DESCSEQ",
        R_ARM_THM_TLS_CALL => "R_ARM_THM_TLS_CALL",
        94 => "R_ARM_PLT32_ABS",
        R_ARM_GOT_ABS => "R_ARM_GOT_ABS",
        R_ARM_GOT_PREL => "R_ARM_GOT_PREL",
        97 => "R_ARM_GOT_BREL12",
        98 => "R_ARM_GOTOFF12",
        99 => "R_ARM_GOTRELAX",
        R_ARM_GNU_VTENTRY => "R_ARM_GNU_VTENTRY",
        R_ARM_GNU_VTINHERIT => "R_ARM_GNU_VTINHERIT",
        R_ARM_THM_JUMP11 => "R_ARM_THM_JUMP11",
        R_ARM_THM_JUMP8 => "R_ARM_THM_JUMP8",
        R_ARM_TLS_GD32 => "R_ARM_TLS_GD32",
        R_ARM_TLS_LDM32 => "R_ARM_TLS_LDM32",
        R_ARM_TLS_LDO32 => "R_ARM_TLS_LDO32",
        R_ARM_TLS_IE32 => "R_ARM_TLS_IE32",
        R_ARM_TLS_LE32 => "R_ARM_TLS_LE32",
        109 => "R_ARM_TLS_LDO12",
        110 => "R_ARM_TLS_LE12",
        111 => "R_ARM_TLS_IE12GP",
        R_ARM_THM_TLS_DESCSEQ16 => "R_ARM_THM_TLS_DESCSEQ16",
        R_ARM_THM_TLS_DESCSEQ32 => "R_ARM_THM_TLS_DESCSEQ32",
        R_ARM_IRELATIVE => "R_ARM_IRELATIVE",
        _ => return None,
    })
}

/// What a relocation patches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Patch {
    /// Nothing.
    None,
    /// A little-endian data field of this many bytes.
    Data(u8),
    /// An instruction field.
    Insn(Field),
}

/// The field relocation `r_type` patches.
#[must_use]
pub fn patch_of(r_type: u32) -> Patch {
    match r_type {
        R_ARM_ABS32 | R_ARM_REL32 | R_ARM_TARGET1 | R_ARM_TARGET2 | R_ARM_ABS32_NOI
        | R_ARM_REL32_NOI | R_ARM_GOTOFF32 | R_ARM_BASE_PREL | R_ARM_GOT_BREL | R_ARM_BASE_ABS
        | R_ARM_GOT_ABS | R_ARM_GOT_PREL | R_ARM_TLS_GD32 | R_ARM_TLS_LDM32 | R_ARM_TLS_LDO32
        | R_ARM_TLS_IE32 | R_ARM_TLS_LE32 | R_ARM_TLS_GOTDESC => Patch::Data(4),
        R_ARM_ABS16 => Patch::Data(2),
        R_ARM_ABS8 => Patch::Data(1),
        R_ARM_PREL31 => Patch::Insn(Field::Prel31),
        R_ARM_PC24 | R_ARM_PLT32 | R_ARM_CALL | R_ARM_JUMP24 => Patch::Insn(Field::Branch24),
        R_ARM_THM_CALL | R_ARM_THM_JUMP24 => Patch::Insn(Field::ThumbBranch24),
        R_ARM_THM_JUMP19 => Patch::Insn(Field::ThumbBranch19),
        R_ARM_THM_JUMP11 => Patch::Insn(Field::ThumbBranch11),
        R_ARM_THM_JUMP8 => Patch::Insn(Field::ThumbBranch8),
        R_ARM_MOVW_ABS_NC | R_ARM_MOVW_PREL_NC => Patch::Insn(Field::Movw),
        R_ARM_MOVT_ABS | R_ARM_MOVT_PREL => Patch::Insn(Field::Movt),
        R_ARM_THM_MOVW_ABS_NC | R_ARM_THM_MOVW_PREL_NC => Patch::Insn(Field::ThumbMovw),
        R_ARM_THM_MOVT_ABS | R_ARM_THM_MOVT_PREL => Patch::Insn(Field::ThumbMovt),
        R_ARM_THM_ALU_PREL_11_0 => Patch::Insn(Field::ThumbAdr),
        R_ARM_THM_PC12 => Patch::Insn(Field::ThumbLdrLiteral),
        R_ARM_LDR_PC_G0 => Patch::Insn(Field::LdrLiteral),
        R_ARM_ALU_PC_G0 => Patch::Insn(Field::AluPc { checked: true }),
        R_ARM_ALU_PC_G0_NC => Patch::Insn(Field::AluPc { checked: false }),
        _ => Patch::None,
    }
}

/// The addend of `SHT_REL` relocation `r_type` at `offset` in section
/// `data`: the contents of the field it patches, decoded.
#[must_use]
pub fn implicit_addend(r_type: u32, data: &[u8], offset: u64) -> i64 {
    let Ok(at) = usize::try_from(offset) else {
        return 0;
    };
    match patch_of(r_type) {
        Patch::None => 0,
        Patch::Data(4) => insn::read32(data, at).map_or(0, |w| i64::from(w as i32)),
        Patch::Data(2) => insn::read16(data, at).map_or(0, |h| i64::from(h as i16)),
        Patch::Data(_) => data.get(at).map_or(0, |&b| i64::from(b as i8)),
        Patch::Insn(field) => field.read(data, at).map_or(0, |i| field.decode(i)),
    }
}

const fn class(kind: Kind, width: Width) -> Class {
    Class::new(kind, width)
}

const fn field(kind: Kind, field: Field) -> Class {
    Class::new(kind, Width::Arm(field))
}

const fn got(kind: Kind, slot: GotKind) -> Class {
    Class::new(kind, Width::Any32).through(slot)
}

/// Classifies relocation `r_type`.
///
/// # Errors
///
/// [`ClassifyError::Unsupported`] for types qld does not link (dynamic
/// ones, the Sun-style TLS, GNU TLS descriptors, group relocations past
/// `G0`, and the obsolete ones).
pub fn classify(r_type: u32, context: ClassifyContext) -> Result<Class, ClassifyError> {
    let _ = context;
    use Kind as K;
    use Width as W;
    Ok(match r_type {
        R_ARM_NONE | R_ARM_V4BX | R_ARM_GNU_VTENTRY | R_ARM_GNU_VTINHERIT => {
            class(K::None, W::None)
        }
        R_ARM_ABS32 | R_ARM_TARGET1 | R_ARM_ABS32_NOI => class(K::Abs, W::Any32),
        R_ARM_ABS16 => class(K::Abs, W::Any16),
        R_ARM_ABS8 => class(K::Abs, W::Any8),
        R_ARM_REL32 | R_ARM_REL32_NOI => class(K::Pc, W::Any32),
        R_ARM_PREL31 => field(K::Pc, Field::Prel31),
        R_ARM_PC24 | R_ARM_PLT32 | R_ARM_CALL | R_ARM_JUMP24 => field(K::Pc, Field::Branch24),
        R_ARM_THM_CALL | R_ARM_THM_JUMP24 => field(K::Pc, Field::ThumbBranch24),
        R_ARM_THM_JUMP19 => field(K::Pc, Field::ThumbBranch19),
        R_ARM_THM_JUMP11 => field(K::Pc, Field::ThumbBranch11),
        R_ARM_THM_JUMP8 => field(K::Pc, Field::ThumbBranch8),
        R_ARM_MOVW_ABS_NC => field(K::Abs, Field::Movw),
        R_ARM_MOVT_ABS => field(K::Abs, Field::Movt),
        R_ARM_MOVW_PREL_NC => field(K::Pc, Field::Movw),
        R_ARM_MOVT_PREL => field(K::Pc, Field::Movt),
        R_ARM_THM_MOVW_ABS_NC => field(K::Abs, Field::ThumbMovw),
        R_ARM_THM_MOVT_ABS => field(K::Abs, Field::ThumbMovt),
        R_ARM_THM_MOVW_PREL_NC => field(K::Pc, Field::ThumbMovw),
        R_ARM_THM_MOVT_PREL => field(K::Pc, Field::ThumbMovt),
        R_ARM_THM_ALU_PREL_11_0 => field(K::Pc, Field::ThumbAdr),
        R_ARM_THM_PC12 => field(K::Pc, Field::ThumbLdrLiteral),
        R_ARM_LDR_PC_G0 => field(K::Pc, Field::LdrLiteral),
        R_ARM_ALU_PC_G0 => field(K::Pc, Field::AluPc { checked: true }),
        R_ARM_ALU_PC_G0_NC => field(K::Pc, Field::AluPc { checked: false }),
        R_ARM_BASE_PREL => class(K::GotBasePc, W::Any32),
        R_ARM_GOTOFF32 => class(K::GotRel, W::Any32),
        R_ARM_GOT_BREL => got(K::GotSlotRel, GotKind::Address),
        R_ARM_GOT_PREL | R_ARM_TARGET2 => got(K::Got, GotKind::Address),
        R_ARM_GOT_ABS => got(K::GotAbs, GotKind::Address),
        R_ARM_TLS_GD32 => got(K::Got, GotKind::TlsGd),
        R_ARM_TLS_LDM32 => got(K::Got, GotKind::TlsLd),
        R_ARM_TLS_IE32 => got(K::Got, GotKind::TpOff),
        R_ARM_TLS_LDO32 => class(K::DtpOff, W::Any32),
        R_ARM_TLS_LE32 => class(K::TpOff, W::Any32),
        _ => return Err(ClassifyError::Unsupported),
    })
}

/// Whether relocation `r_type` is a call or jump that goes through the PLT
/// when its symbol is preemptible (lld's `R_PLT_PC` types; a `PREL31`
/// personality routine reference in `.ARM.extab` is one too).
#[must_use]
pub fn is_branch(r_type: u32) -> bool {
    matches!(
        r_type,
        R_ARM_PC24
            | R_ARM_PLT32
            | R_ARM_CALL
            | R_ARM_JUMP24
            | R_ARM_THM_CALL
            | R_ARM_THM_JUMP24
            | R_ARM_THM_JUMP19
            | R_ARM_PREL31
    )
}

/// Whether relocation `r_type` is a branch that thunks serve (range
/// extension and interworking).
#[must_use]
pub fn is_thunk_branch(r_type: u32) -> bool {
    matches!(
        r_type,
        R_ARM_PC24
            | R_ARM_PLT32
            | R_ARM_CALL
            | R_ARM_JUMP24
            | R_ARM_THM_CALL
            | R_ARM_THM_JUMP24
            | R_ARM_THM_JUMP19
    )
}

/// Whether branch relocation `r_type` is in Thumb code.
#[must_use]
pub fn is_thumb_branch(r_type: u32) -> bool {
    matches!(
        r_type,
        R_ARM_THM_CALL | R_ARM_THM_JUMP24 | R_ARM_THM_JUMP19 | R_ARM_THM_JUMP11 | R_ARM_THM_JUMP8
    )
}

/// How far the PC reads ahead of the instruction relocation `r_type`
/// patches: 4 in Thumb code, 8 in A32.
#[must_use]
pub fn pc_bias(r_type: u32) -> u64 {
    if is_thumb_branch(r_type) {
        insn::THUMB_PC_BIAS
    } else {
        insn::ARM_PC_BIAS
    }
}

/// A direct branch, as thunk planning and the writer both see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Branch {
    /// The relocation type ([`is_thunk_branch`]).
    pub r_type: u32,
    /// The instruction as the input has it (to tell `bl` from `blx`).
    pub insn: u32,
    /// The address of the instruction.
    pub place: u64,
    /// Where the branch goes: `S + A` plus the PC bias, bit 0 set for a
    /// Thumb function.
    pub destination: u64,
    /// The destination is a function (`STT_FUNC` or `STT_GNU_IFUNC`), so
    /// bit 0 of its address gives its state; otherwise the branch keeps
    /// the state its instruction has.
    pub function: bool,
    /// The branch goes to a PLT entry or IFUNC stub, which is A32.
    pub via_stub: bool,
}

/// How a branch is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BranchPlan {
    /// The destination is Thumb code.
    pub thumb_target: bool,
    /// The branch cannot reach its destination directly (range, or a
    /// change of state its instruction cannot make): it goes through the
    /// thunk of [`thunk_key`].
    pub thunk: bool,
}

impl Branch {
    /// Whether the branch is in Thumb code.
    #[must_use]
    pub fn thumb_caller(&self) -> bool {
        is_thumb_branch(self.r_type)
    }

    /// Where the destination is and whether it can be reached directly.
    #[must_use]
    pub fn plan(&self) -> BranchPlan {
        let caller = self.thumb_caller();
        let thumb_target = if self.via_stub {
            false
        } else if self.function {
            self.destination & 1 != 0
        } else {
            match self.r_type {
                R_ARM_CALL => insn::is_arm_blx(self.insn),
                R_ARM_THM_CALL => !insn::is_thumb_blx(self.insn),
                _ => caller,
            }
        };
        let can_switch = matches!(self.r_type, R_ARM_CALL | R_ARM_THM_CALL);
        let thunk = (thumb_target != caller && !can_switch)
            || !self.in_range(self.destination, thumb_target);
        BranchPlan {
            thumb_target,
            thunk,
        }
    }

    /// Whether the branch reaches `destination` (a Thumb one when
    /// `thumb_target`) directly.
    #[must_use]
    pub fn in_range(&self, destination: u64, thumb_target: bool) -> bool {
        let mut source = self.place.wrapping_add(pc_bias(self.r_type));
        let destination = if thumb_target {
            destination & !1
        } else {
            // `blx` from Thumb branches relative to the aligned PC.
            source &= !3;
            destination
        };
        let offset = i64::from(destination.wrapping_sub(source) as u32 as i32);
        let bits = match self.r_type {
            R_ARM_THM_JUMP19 => 21,
            R_ARM_THM_CALL | R_ARM_THM_JUMP24 => 25,
            _ => 26,
        };
        crate::arch::aarch64::fits_signed(offset, bits)
    }
}

/// The thunk key of a branch to `destination` (bit 0 set for Thumb) from
/// code in the Thumb state when `thumb_caller`: thunks are shared by
/// destination and caller state; `pic` selects the position-independent
/// form. The key is what [`write_thunk`] decodes.
#[must_use]
pub fn thunk_key(destination: u64, thumb_caller: bool, pic: bool) -> u64 {
    (destination & 0xffff_ffff) | (u64::from(thumb_caller) << 32) | (u64::from(pic) << 33)
}

/// Writes the thunk of key `key` ([`thunk_key`]) at address `address` into
/// `out` at `at`.
///
/// # Errors
///
/// [`ApplyError`] when out of bounds.
pub fn write_thunk(out: &mut [u8], at: u64, address: u64, key: u64) -> Result<(), ApplyError> {
    let start = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    let slot = out.get_mut(start..).ok_or(ApplyError::OutOfBounds)?;
    let kind = insn::ThunkKind {
        thumb: key & (1 << 32) != 0,
        pic: key & (1 << 33) != 0,
    };
    insn::write_thunk(slot, address, key & 0xffff_ffff, kind).map_err(|_| ApplyError::Overflow)
}

fn put_words(out: &mut [u8], words: &[u32]) -> Result<(), ApplyError> {
    for (index, &word) in words.iter().enumerate() {
        let at = index.checked_mul(4).ok_or(ApplyError::OutOfBounds)?;
        insn::write32(out, at, word).ok_or(ApplyError::OutOfBounds)?;
    }
    Ok(())
}

/// Writes the PLT header at `plt`, which jumps to the resolver through
/// `.got.plt` at `got_plt`.
///
/// # Errors
///
/// [`ApplyError::OutOfBounds`] when `out` is too small.
pub fn write_plt_header(out: &mut [u8], plt: u64, got_plt: u64) -> Result<(), ApplyError> {
    put_words(out, &insn::plt_header(plt, got_plt))
}

/// Writes a PLT entry (or IFUNC stub) at `entry` that jumps through the
/// GOT word at `slot`.
///
/// # Errors
///
/// [`ApplyError::Overflow`] when the slot is out of the entry's reach.
pub fn write_plt_entry(out: &mut [u8], entry: u64, slot: u64) -> Result<(), ApplyError> {
    let words = insn::plt_entry(entry, slot).map_err(|_| ApplyError::Overflow)?;
    put_words(out, &words)
}

/// The `e_flags` of the output: EABI version 5, with the float ABI of the
/// merged attributes (hard when arguments go in VFP registers).
#[must_use]
pub fn output_flags(hard_float: bool) -> u32 {
    EF_ARM_EABI_VER5
        | if hard_float {
            EF_ARM_ABI_FLOAT_HARD
        } else {
            EF_ARM_ABI_FLOAT_SOFT
        }
}

/// The program interpreter: glibc's hard-float dynamic linker.
pub const INTERPRETER: &str = "/lib/ld-linux-armhf.so.3";

/// What the Arm backend builds before layout, because it changes the size
/// of input sections: the merged `.ARM.attributes` and the exception
/// index, each of which takes the place of the first input section of its
/// kind ([`apply::member_size`]).
#[derive(Clone, Debug, Default)]
pub struct Prepared {
    /// The merged `.ARM.attributes`.
    pub attributes: Option<attributes::Output>,
    /// The exception index.
    pub exidx: Option<exidx::Table>,
}

/// Builds them. `merge_exidx` drops exception index entries that repeat
/// the one before, which is only safe when layout will place the code in
/// the order [`exidx::plan`] assumes.
#[must_use]
pub fn prepare<F: crate::elf::read::ElfFormat>(
    refs: &crate::elf::refs::Refs<'_, '_, F>,
    placement: &crate::elf::place::Placement<'_>,
    merge_exidx: bool,
) -> Prepared {
    Prepared {
        attributes: attributes::collect(refs),
        exidx: exidx::plan(refs, placement, merge_exidx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ClassifyContext {
        ClassifyContext::static_exec(true)
    }

    #[test]
    fn classifies_the_common_relocations() {
        let c = |r| classify(r, context()).unwrap();
        assert_eq!(c(R_ARM_ABS32), class(Kind::Abs, Width::Any32));
        assert_eq!(c(R_ARM_TARGET1), class(Kind::Abs, Width::Any32));
        assert_eq!(c(R_ARM_TARGET2).kind, Kind::Got);
        assert_eq!(c(R_ARM_TLS_GD32).slot, GotKind::TlsGd);
        assert_eq!(c(R_ARM_TLS_IE32).slot, GotKind::TpOff);
        assert_eq!(c(R_ARM_V4BX).kind, Kind::None);
        assert_eq!(c(R_ARM_THM_CALL).width, Width::Arm(Field::ThumbBranch24));
        assert!(classify(R_ARM_TLS_GOTDESC, context()).is_err());
        assert!(classify(R_ARM_RELATIVE, context()).is_err());
        assert_eq!(reloc_name(R_ARM_THM_JUMP24), Some("R_ARM_THM_JUMP24"));
    }

    #[test]
    fn reads_implicit_addends() {
        let mut data = vec![0u8; 16];
        insn::write32(&mut data, 0, 0xebff_fffe);
        insn::write_thumb32(&mut data, 4, 0xf7ff_fffe);
        insn::write32(&mut data, 8, 0xffff_fffc);
        insn::write32(&mut data, 12, 0x7fff_fff0);
        assert_eq!(implicit_addend(R_ARM_CALL, &data, 0), -8);
        assert_eq!(implicit_addend(R_ARM_THM_CALL, &data, 4), -4);
        assert_eq!(implicit_addend(R_ARM_ABS32, &data, 8), -4);
        assert_eq!(implicit_addend(R_ARM_PREL31, &data, 12), -16);
        assert_eq!(implicit_addend(R_ARM_ABS32, &data, 14), 0);
    }

    fn branch(r_type: u32, insn: u32, destination: u64, function: bool) -> Branch {
        Branch {
            r_type,
            insn,
            place: 0x1_0000,
            destination,
            function,
            via_stub: false,
        }
    }

    #[test]
    fn plans_interworking() {
        // bl to a Thumb function: blx, no thunk.
        let plan = branch(R_ARM_CALL, 0xebff_fffe, 0x2_0001, true).plan();
        assert!(plan.thumb_target && !plan.thunk);
        // b to a Thumb function: thunk.
        let plan = branch(R_ARM_JUMP24, 0xeaff_fffe, 0x2_0001, true).plan();
        assert!(plan.thumb_target && plan.thunk);
        // Thumb bl to an A32 function: blx.
        let plan = branch(R_ARM_THM_CALL, 0xf7ff_fffe, 0x2_0000, true).plan();
        assert!(!plan.thumb_target && !plan.thunk);
        // Thumb b.w to a PLT entry: thunk.
        let mut via_plt = branch(R_ARM_THM_JUMP24, 0xf7ff_bffe, 0x2_0000, false);
        via_plt.via_stub = true;
        assert!(via_plt.plan().thunk);
        // A label without a type keeps the instruction's state.
        let plan = branch(R_ARM_THM_CALL, 0xf7ff_effe, 0x2_0000, false).plan();
        assert!(!plan.thumb_target && !plan.thunk);
        // Out of range.
        let plan = branch(R_ARM_CALL, 0xebff_fffe, 0x300_0000, true).plan();
        assert!(plan.thunk);
        let plan = branch(R_ARM_THM_CALL, 0xf7ff_fffe, 0x102_0001, true).plan();
        assert!(plan.thunk);
        let plan = branch(R_ARM_THM_CALL, 0xf7ff_fffe, 0x100_0001, true).plan();
        assert!(!plan.thunk);
    }

    #[test]
    fn thunk_keys_carry_the_state() {
        let key = thunk_key(0x2_0001, true, false);
        let mut out = [0u8; 16];
        write_thunk(&mut out, 0, 0x1000, key).unwrap();
        assert_eq!(insn::read16(&out, 8), Some(insn::THUMB_BX_IP));
        let key = thunk_key(0x2_0000, false, true);
        write_thunk(&mut out, 0, 0x1000, key).unwrap();
        assert_eq!(insn::read32(&out, 12), Some(insn::BX_IP));
    }
}
