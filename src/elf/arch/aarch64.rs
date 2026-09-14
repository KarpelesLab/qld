//! AArch64 relocations: classification, TLS relaxation and the PLT entry
//! encodings.
//!
//! [`classify`] turns a relocation type into a [`Class`]: what the ABI says
//! to compute ([`Kind`]), which GOT entry it reads ([`GotKind`]) and which
//! instruction field holds the result ([`Field`]). The relocation scan and
//! the writer both use it, so they always agree.
//!
//! TLS follows the psABI and GNU ld: in an executable, general-dynamic,
//! local-dynamic and descriptor accesses to a variable of the executable
//! become local-exec, accesses to a shared library's variable become
//! initial-exec, and initial-exec becomes local-exec. The sequences are
//! rewritten instruction by instruction, each one identified by its own
//! relocation, except the `bl __tls_get_addr` of a general-dynamic access
//! (and the `nop` after it), which the `ADD_LO12_NC` of the sequence
//! rewrites and which is then skipped.
//!
//! The PLT is GNU ld's: a 32-byte header that saves `x16`/`x30` and jumps
//! through `.got.plt[2]`, then one 16-byte entry per symbol. With BTI the
//! header starts with `bti c`, because the entries reach it through
//! `br x17`; the entries themselves are only reached by direct branches
//! and keep their 16-byte form, as in GNU ld.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::aarch64::{self as insn, Field, MovwCheck, NOP, page, read_insn, write_insn};
use crate::elf::read::consts::aarch64::*;

use super::{
    ApplyError, Class, ClassifyContext, ClassifyError, GotKind, Kind, PltFlags, RelaxValues,
    TlsMode, Width,
};

const fn class(kind: Kind, width: Width) -> Class {
    Class::new(kind, width)
}

const fn got(kind: Kind, width: Width, slot: GotKind) -> Class {
    Class::new(kind, width).through(slot)
}

const fn field(f: Field) -> Width {
    Width::Field(f)
}

const fn ldst(scale: u8, checked: bool) -> Width {
    Width::Field(Field::LdSt { scale, checked })
}

const fn movw(shift: u8, check: MovwCheck) -> Width {
    Width::Field(Field::Movw { shift, check })
}

/// The `MOVW` group of a `_G<n>` relocation, unchecked.
const fn movw_nc(shift: u8) -> Width {
    movw(shift, MovwCheck::None)
}

/// Classifies AArch64 relocation `r_type`.
///
/// # Errors
///
/// [`ClassifyError::Unsupported`] for types qld does not link, including
/// the dynamic-only types and the pointer-authentication (`AUTH_*`) set.
#[allow(clippy::too_many_lines)]
pub fn classify(r_type: u32, context: ClassifyContext) -> Result<Class, ClassifyError> {
    use Kind as K;
    use Width as W;
    // How a TLS access to this symbol is linked.
    let tls = context.tls;
    let tls_ld = context.tls_ld;
    Ok(match r_type {
        R_AARCH64_NONE | R_AARCH64_NULL => class(K::None, W::None),

        // Data.
        R_AARCH64_ABS64 => class(K::Abs, W::W64),
        R_AARCH64_ABS32 => class(K::Abs, field(Field::Data32)),
        R_AARCH64_ABS16 => class(K::Abs, field(Field::Data16)),
        R_AARCH64_PREL64 => class(K::Pc, W::W64),
        R_AARCH64_PREL32 => class(K::Pc, field(Field::Data32)),
        R_AARCH64_PREL16 => class(K::Pc, field(Field::Data16)),
        R_AARCH64_PLT32 => class(K::Pc, field(Field::Data32Signed)),

        // Absolute MOVW groups.
        R_AARCH64_MOVW_UABS_G0 => class(K::Abs, movw(0, MovwCheck::Unsigned)),
        R_AARCH64_MOVW_UABS_G0_NC => class(K::Abs, movw_nc(0)),
        R_AARCH64_MOVW_UABS_G1 => class(K::Abs, movw(16, MovwCheck::Unsigned)),
        R_AARCH64_MOVW_UABS_G1_NC => class(K::Abs, movw_nc(16)),
        R_AARCH64_MOVW_UABS_G2 => class(K::Abs, movw(32, MovwCheck::Unsigned)),
        R_AARCH64_MOVW_UABS_G2_NC => class(K::Abs, movw_nc(32)),
        R_AARCH64_MOVW_UABS_G3 => class(K::Abs, movw_nc(48)),
        R_AARCH64_MOVW_SABS_G0 => class(K::Abs, movw(0, MovwCheck::Signed)),
        R_AARCH64_MOVW_SABS_G1 => class(K::Abs, movw(16, MovwCheck::Signed)),
        R_AARCH64_MOVW_SABS_G2 => class(K::Abs, movw(32, MovwCheck::Signed)),

        // PC-relative MOVW groups.
        R_AARCH64_MOVW_PREL_G0 => class(K::Pc, movw(0, MovwCheck::Signed)),
        R_AARCH64_MOVW_PREL_G0_NC => class(K::Pc, movw_nc(0)),
        R_AARCH64_MOVW_PREL_G1 => class(K::Pc, movw(16, MovwCheck::Signed)),
        R_AARCH64_MOVW_PREL_G1_NC => class(K::Pc, movw_nc(16)),
        R_AARCH64_MOVW_PREL_G2 => class(K::Pc, movw(32, MovwCheck::Signed)),
        R_AARCH64_MOVW_PREL_G2_NC => class(K::Pc, movw_nc(32)),
        R_AARCH64_MOVW_PREL_G3 => class(K::Pc, movw_nc(48)),

        // PC-relative addressing and branches.
        R_AARCH64_LD_PREL_LO19 => class(K::Pc, field(Field::Branch19)),
        R_AARCH64_ADR_PREL_LO21 => class(K::Pc, field(Field::Adr21)),
        R_AARCH64_ADR_PREL_PG_HI21 => class(K::Page, field(Field::Adrp21)),
        R_AARCH64_ADR_PREL_PG_HI21_NC => class(K::Page, field(Field::Adrp21Nc)),
        R_AARCH64_ADD_ABS_LO12_NC => class(K::Abs, field(Field::Add12)),
        R_AARCH64_LDST8_ABS_LO12_NC => class(K::Abs, ldst(0, false)),
        R_AARCH64_LDST16_ABS_LO12_NC => class(K::Abs, ldst(1, false)),
        R_AARCH64_LDST32_ABS_LO12_NC => class(K::Abs, ldst(2, false)),
        R_AARCH64_LDST64_ABS_LO12_NC => class(K::Abs, ldst(3, false)),
        R_AARCH64_LDST128_ABS_LO12_NC => class(K::Abs, ldst(4, false)),
        R_AARCH64_TSTBR14 => class(K::Pc, field(Field::Branch14)),
        R_AARCH64_CONDBR19 => class(K::Pc, field(Field::Branch19)),
        R_AARCH64_JUMP26 | R_AARCH64_CALL26 => class(K::Pc, field(Field::Branch26)),

        // GOT.
        R_AARCH64_GOT_LD_PREL19 => class(K::Got, field(Field::Branch19)),
        R_AARCH64_ADR_GOT_PAGE => class(K::GotPage, field(Field::Adrp21)),
        R_AARCH64_LD64_GOT_LO12_NC => class(K::GotAbs, ldst(3, false)),
        R_AARCH64_LD64_GOTPAGE_LO15 => class(K::GotPageOff, field(Field::LdSt15)),
        R_AARCH64_GOTPCREL32 => class(K::Got, field(Field::Data32Signed)),
        R_AARCH64_GOTREL64 => class(K::GotRel, W::W64),
        R_AARCH64_GOTREL32 => class(K::GotRel, field(Field::Data32)),
        R_AARCH64_LD64_GOTOFF_LO15 => class(K::GotSlotRel, field(Field::LdSt15)),
        R_AARCH64_MOVW_GOTOFF_G0 => class(K::GotSlotRel, movw(0, MovwCheck::Unsigned)),
        R_AARCH64_MOVW_GOTOFF_G0_NC => class(K::GotSlotRel, movw_nc(0)),
        R_AARCH64_MOVW_GOTOFF_G1 => class(K::GotSlotRel, movw(16, MovwCheck::Unsigned)),
        R_AARCH64_MOVW_GOTOFF_G1_NC => class(K::GotSlotRel, movw_nc(16)),
        R_AARCH64_MOVW_GOTOFF_G2 => class(K::GotSlotRel, movw(32, MovwCheck::Unsigned)),
        R_AARCH64_MOVW_GOTOFF_G2_NC => class(K::GotSlotRel, movw_nc(32)),
        R_AARCH64_MOVW_GOTOFF_G3 => class(K::GotSlotRel, movw_nc(48)),

        // Local-exec TLS: always a thread pointer offset.
        R_AARCH64_TLSLE_MOVW_TPREL_G2 => class(K::TpOff, movw(32, MovwCheck::Signed)),
        R_AARCH64_TLSLE_MOVW_TPREL_G1 => class(K::TpOff, movw(16, MovwCheck::Signed)),
        R_AARCH64_TLSLE_MOVW_TPREL_G1_NC => class(K::TpOff, movw_nc(16)),
        R_AARCH64_TLSLE_MOVW_TPREL_G0 => class(K::TpOff, movw(0, MovwCheck::Signed)),
        R_AARCH64_TLSLE_MOVW_TPREL_G0_NC => class(K::TpOff, movw_nc(0)),
        R_AARCH64_TLSLE_ADD_TPREL_HI12 => class(K::TpOff, field(Field::AddHi12)),
        R_AARCH64_TLSLE_ADD_TPREL_LO12 => class(K::TpOff, field(Field::Add12Checked)),
        R_AARCH64_TLSLE_ADD_TPREL_LO12_NC => class(K::TpOff, field(Field::Add12)),
        R_AARCH64_TLSLE_LDST8_TPREL_LO12 => class(K::TpOff, ldst(0, true)),
        R_AARCH64_TLSLE_LDST8_TPREL_LO12_NC => class(K::TpOff, ldst(0, false)),
        R_AARCH64_TLSLE_LDST16_TPREL_LO12 => class(K::TpOff, ldst(1, true)),
        R_AARCH64_TLSLE_LDST16_TPREL_LO12_NC => class(K::TpOff, ldst(1, false)),
        R_AARCH64_TLSLE_LDST32_TPREL_LO12 => class(K::TpOff, ldst(2, true)),
        R_AARCH64_TLSLE_LDST32_TPREL_LO12_NC => class(K::TpOff, ldst(2, false)),
        R_AARCH64_TLSLE_LDST64_TPREL_LO12 => class(K::TpOff, ldst(3, true)),
        R_AARCH64_TLSLE_LDST64_TPREL_LO12_NC => class(K::TpOff, ldst(3, false)),
        R_AARCH64_TLSLE_LDST128_TPREL_LO12 => class(K::TpOff, ldst(4, true)),
        R_AARCH64_TLSLE_LDST128_TPREL_LO12_NC => class(K::TpOff, ldst(4, false)),

        // Local-dynamic offsets inside the module's TLS block.
        R_AARCH64_TLSLD_MOVW_DTPREL_G2 => class(K::DtpOff, movw(32, MovwCheck::Signed)),
        R_AARCH64_TLSLD_MOVW_DTPREL_G1 => class(K::DtpOff, movw(16, MovwCheck::Signed)),
        R_AARCH64_TLSLD_MOVW_DTPREL_G1_NC => class(K::DtpOff, movw_nc(16)),
        R_AARCH64_TLSLD_MOVW_DTPREL_G0 => class(K::DtpOff, movw(0, MovwCheck::Signed)),
        R_AARCH64_TLSLD_MOVW_DTPREL_G0_NC => class(K::DtpOff, movw_nc(0)),
        R_AARCH64_TLSLD_ADD_DTPREL_HI12 => class(K::DtpOff, field(Field::AddHi12)),
        R_AARCH64_TLSLD_ADD_DTPREL_LO12 => class(K::DtpOff, field(Field::Add12Checked)),
        R_AARCH64_TLSLD_ADD_DTPREL_LO12_NC => class(K::DtpOff, field(Field::Add12)),
        R_AARCH64_TLSLD_LDST8_DTPREL_LO12 => class(K::DtpOff, ldst(0, true)),
        R_AARCH64_TLSLD_LDST8_DTPREL_LO12_NC => class(K::DtpOff, ldst(0, false)),
        R_AARCH64_TLSLD_LDST16_DTPREL_LO12 => class(K::DtpOff, ldst(1, true)),
        R_AARCH64_TLSLD_LDST16_DTPREL_LO12_NC => class(K::DtpOff, ldst(1, false)),
        R_AARCH64_TLSLD_LDST32_DTPREL_LO12 => class(K::DtpOff, ldst(2, true)),
        R_AARCH64_TLSLD_LDST32_DTPREL_LO12_NC => class(K::DtpOff, ldst(2, false)),
        R_AARCH64_TLSLD_LDST64_DTPREL_LO12 => class(K::DtpOff, ldst(3, true)),
        R_AARCH64_TLSLD_LDST64_DTPREL_LO12_NC => class(K::DtpOff, ldst(3, false)),
        R_AARCH64_TLSLD_LDST128_DTPREL_LO12 => class(K::DtpOff, ldst(4, true)),
        R_AARCH64_TLSLD_LDST128_DTPREL_LO12_NC => class(K::DtpOff, ldst(4, false)),

        // General-dynamic.
        R_AARCH64_TLSGD_ADR_PAGE21 => match tls {
            TlsMode::Dynamic => got(K::GotPage, field(Field::Adrp21), GotKind::TlsGd),
            TlsMode::LocalExec => class(K::GdToLe, W::None),
            TlsMode::InitialExec => class(K::GdToIe, W::None),
        },
        R_AARCH64_TLSGD_ADD_LO12_NC => match tls {
            TlsMode::Dynamic => got(K::GotAbs, field(Field::Add12), GotKind::TlsGd),
            TlsMode::LocalExec => class(K::GdToLe, W::None).skipping(),
            TlsMode::InitialExec => class(K::GdToIe, W::None).skipping(),
        },
        R_AARCH64_TLSGD_ADR_PREL21 => match tls {
            TlsMode::Dynamic => got(K::Got, field(Field::Adr21), GotKind::TlsGd),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        R_AARCH64_TLSGD_MOVW_G1 => match tls {
            TlsMode::Dynamic => got(K::GotSlotRel, movw(16, MovwCheck::Signed), GotKind::TlsGd),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        R_AARCH64_TLSGD_MOVW_G0_NC => match tls {
            TlsMode::Dynamic => got(K::GotSlotRel, movw_nc(0), GotKind::TlsGd),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },

        // Local-dynamic addressing of the module's TLS pair.
        R_AARCH64_TLSLD_ADR_PAGE21 => match tls_ld {
            TlsMode::Dynamic => got(K::GotPage, field(Field::Adrp21), GotKind::TlsLd),
            _ => class(K::LdToLe, W::None),
        },
        R_AARCH64_TLSLD_ADD_LO12_NC => match tls_ld {
            TlsMode::Dynamic => got(K::GotAbs, field(Field::Add12), GotKind::TlsLd),
            _ => class(K::LdToLe, W::None).skipping(),
        },
        R_AARCH64_TLSLD_ADR_PREL21 => match tls_ld {
            TlsMode::Dynamic => got(K::Got, field(Field::Adr21), GotKind::TlsLd),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        R_AARCH64_TLSLD_LD_PREL19 => match tls_ld {
            TlsMode::Dynamic => got(K::Got, field(Field::Branch19), GotKind::TlsLd),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        R_AARCH64_TLSLD_MOVW_G1 => match tls_ld {
            TlsMode::Dynamic => got(K::GotSlotRel, movw(16, MovwCheck::Signed), GotKind::TlsLd),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        R_AARCH64_TLSLD_MOVW_G0_NC => match tls_ld {
            TlsMode::Dynamic => got(K::GotSlotRel, movw_nc(0), GotKind::TlsLd),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },

        // Initial-exec.
        R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21 => match tls {
            TlsMode::LocalExec => class(K::IeToLe, W::None),
            _ => got(K::GotPage, field(Field::Adrp21), GotKind::TpOff),
        },
        R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC => match tls {
            TlsMode::LocalExec => class(K::IeToLe, W::None),
            _ => got(K::GotAbs, ldst(3, false), GotKind::TpOff),
        },
        R_AARCH64_TLSIE_LD_GOTTPREL_PREL19 => match tls {
            TlsMode::LocalExec => class(K::IeToLe, W::None),
            _ => got(K::Got, field(Field::Branch19), GotKind::TpOff),
        },
        R_AARCH64_TLSIE_MOVW_GOTTPREL_G1 => match tls {
            TlsMode::LocalExec => class(K::IeToLe, W::None),
            _ => got(K::GotSlotRel, movw(16, MovwCheck::Signed), GotKind::TpOff),
        },
        R_AARCH64_TLSIE_MOVW_GOTTPREL_G0_NC => match tls {
            TlsMode::LocalExec => class(K::IeToLe, W::None),
            _ => got(K::GotSlotRel, movw_nc(0), GotKind::TpOff),
        },

        // TLS descriptors.
        R_AARCH64_TLSDESC_ADR_PAGE21 => match tls {
            TlsMode::Dynamic => got(K::GotPage, field(Field::Adrp21), GotKind::TlsDesc),
            TlsMode::LocalExec => class(K::DescToLe, W::None),
            TlsMode::InitialExec => class(K::DescToIe, W::None),
        },
        R_AARCH64_TLSDESC_LD64_LO12 => match tls {
            TlsMode::Dynamic => got(K::GotAbs, ldst(3, false), GotKind::TlsDesc),
            TlsMode::LocalExec => class(K::DescToLe, W::None),
            TlsMode::InitialExec => class(K::DescToIe, W::None),
        },
        R_AARCH64_TLSDESC_ADD_LO12 => match tls {
            TlsMode::Dynamic => got(K::GotAbs, field(Field::Add12), GotKind::TlsDesc),
            TlsMode::LocalExec => class(K::DescToLe, W::None),
            TlsMode::InitialExec => class(K::DescToIe, W::None),
        },
        R_AARCH64_TLSDESC_CALL => match tls {
            TlsMode::Dynamic => class(K::None, W::None),
            _ => class(K::DescCallToLe, W::None),
        },
        R_AARCH64_TLSDESC_ADR_PREL21 => match tls {
            TlsMode::Dynamic => got(K::Got, field(Field::Adr21), GotKind::TlsDesc),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        R_AARCH64_TLSDESC_LD_PREL19 => match tls {
            TlsMode::Dynamic => got(K::Got, field(Field::Branch19), GotKind::TlsDesc),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        R_AARCH64_TLSDESC_OFF_G1 => match tls {
            TlsMode::Dynamic => got(K::GotSlotRel, movw(16, MovwCheck::Signed), GotKind::TlsDesc),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        R_AARCH64_TLSDESC_OFF_G0_NC => match tls {
            TlsMode::Dynamic => got(K::GotSlotRel, movw_nc(0), GotKind::TlsDesc),
            _ => return Err(ClassifyError::BadTlsInstruction),
        },
        // Optimization hints, not relocations of a field.
        R_AARCH64_TLSDESC_LDR | R_AARCH64_TLSDESC_ADD => class(K::None, W::None),

        _ => return Err(ClassifyError::Unsupported),
    })
}

fn word(out: &mut [u8], at: u64) -> Result<usize, ApplyError> {
    let at = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    if read_insn(out, at).is_none() {
        return Err(ApplyError::OutOfBounds);
    }
    Ok(at)
}

fn put(out: &mut [u8], at: u64, value: u32) -> Result<(), ApplyError> {
    let at = word(out, at)?;
    write_insn(out, at, value).ok_or(ApplyError::OutOfBounds)
}

fn get(out: &[u8], at: u64) -> Result<u32, ApplyError> {
    let at = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    read_insn(out, at).ok_or(ApplyError::OutOfBounds)
}

fn patch(out: &mut [u8], at: u64, insn: u32, field: Field, value: i64) -> Result<(), ApplyError> {
    let encoded = field
        .encode(insn, value)
        .map_err(|_| ApplyError::Overflow)?;
    put(out, at, encoded)
}

/// `add x0, x1, x0`: the thread pointer plus the offset a relaxed
/// general-dynamic sequence computed.
const ADD_X0_X1_X0: u32 = 0x8b00_0020;

/// Whether a general-dynamic sequence whose `add` is at `at` is followed by
/// `bl __tls_get_addr; nop`, the form GCC and clang emit and the one GNU ld
/// relaxes into four instructions.
fn gd_tail_is_call_nop(out: &[u8], at: u64) -> bool {
    let call = at.checked_add(4).and_then(|o| get(out, o).ok());
    let nop = at.checked_add(8).and_then(|o| get(out, o).ok());
    call.is_some_and(|c| c & 0xfc00_0000 == 0x9400_0000) && nop == Some(NOP)
}

/// Rewrites one instruction of a relaxed TLS sequence.
///
/// # Errors
///
/// [`ApplyError`] for sequences qld cannot rewrite, and for offsets that
/// do not fit the replacement instructions.
#[allow(clippy::too_many_lines)]
pub fn relax_tls(
    out: &mut [u8],
    offset: u64,
    kind: Kind,
    r_type: u32,
    values: RelaxValues,
) -> Result<(), ApplyError> {
    let tpoff = values.tpoff;
    let hi16 = |v: i64| ((v as u64) >> 16) as u32 & 0xffff;
    let lo16 = |v: i64| (v as u64) as u32 & 0xffff;
    let page_delta = |target: u64| (page(target) as i64).wrapping_sub(page(values.place) as i64);
    match kind {
        Kind::IeToLe => {
            if !insn::fits_unsigned(tpoff, 32) {
                return Err(ApplyError::Overflow);
            }
            let register = insn::destination_register(get(out, offset)?);
            match r_type {
                R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21 => {
                    put(out, offset, insn::movz(register, hi16(tpoff), 16))
                }
                R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC => {
                    put(out, offset, insn::movk(register, lo16(tpoff), 0))
                }
                R_AARCH64_TLSIE_MOVW_GOTTPREL_G1 => {
                    let insn = get(out, offset)?;
                    patch(
                        out,
                        offset,
                        insn,
                        Field::Movw {
                            shift: 16,
                            check: MovwCheck::None,
                        },
                        tpoff,
                    )
                }
                R_AARCH64_TLSIE_MOVW_GOTTPREL_G0_NC => {
                    let insn = get(out, offset)?;
                    patch(
                        out,
                        offset,
                        insn,
                        Field::Movw {
                            shift: 0,
                            check: MovwCheck::None,
                        },
                        tpoff,
                    )
                }
                _ => Err(ApplyError::BadInstruction),
            }
        }
        Kind::DescToLe | Kind::DescCallToLe => {
            if !insn::fits_unsigned(tpoff, 32) {
                return Err(ApplyError::Overflow);
            }
            match r_type {
                R_AARCH64_TLSDESC_ADR_PAGE21 => put(out, offset, insn::movz(0, hi16(tpoff), 16)),
                R_AARCH64_TLSDESC_LD64_LO12 => put(out, offset, insn::movk(0, lo16(tpoff), 0)),
                R_AARCH64_TLSDESC_ADD_LO12 | R_AARCH64_TLSDESC_CALL => put(out, offset, NOP),
                _ => Err(ApplyError::BadInstruction),
            }
        }
        Kind::DescToIe => match r_type {
            R_AARCH64_TLSDESC_ADR_PAGE21 => patch(
                out,
                offset,
                insn::adrp(0),
                Field::Adrp21,
                page_delta(values.got),
            ),
            R_AARCH64_TLSDESC_LD64_LO12 => patch(
                out,
                offset,
                insn::ldr_offset(0, 0),
                Field::LdSt {
                    scale: 3,
                    checked: false,
                },
                values.got as i64,
            ),
            R_AARCH64_TLSDESC_ADD_LO12 | R_AARCH64_TLSDESC_CALL => put(out, offset, NOP),
            _ => Err(ApplyError::BadInstruction),
        },
        Kind::GdToLe => match r_type {
            R_AARCH64_TLSGD_ADR_PAGE21 => {
                if gd_tail_is_call_nop(out, offset.wrapping_add(4)) {
                    if !insn::fits_unsigned(tpoff, 32) {
                        return Err(ApplyError::Overflow);
                    }
                    put(out, offset, insn::movz(0, hi16(tpoff), 16))
                } else {
                    put(out, offset, insn::MRS_TPIDR_X0)
                }
            }
            R_AARCH64_TLSGD_ADD_LO12_NC => {
                if gd_tail_is_call_nop(out, offset) {
                    if !insn::fits_unsigned(tpoff, 32) {
                        return Err(ApplyError::Overflow);
                    }
                    put(out, offset, insn::movk(0, lo16(tpoff), 0))?;
                    put(out, offset.wrapping_add(4), insn::MRS_TPIDR_X0 | 1)?;
                    put(out, offset.wrapping_add(8), ADD_X0_X1_X0)
                } else {
                    // Without the trailing `nop` there are only three slots:
                    // `mrs; add hi12; add lo12`, which needs a 24-bit offset.
                    if !insn::fits_unsigned(tpoff, 24) {
                        return Err(ApplyError::Overflow);
                    }
                    patch(out, offset, insn::add_imm(0, 0), Field::AddHi12, tpoff)?;
                    patch(
                        out,
                        offset.wrapping_add(4),
                        insn::add_imm(0, 0),
                        Field::Add12,
                        tpoff,
                    )
                }
            }
            _ => Err(ApplyError::BadInstruction),
        },
        Kind::GdToIe => match r_type {
            R_AARCH64_TLSGD_ADR_PAGE21 => patch(
                out,
                offset,
                insn::adrp(0),
                Field::Adrp21,
                page_delta(values.got),
            ),
            R_AARCH64_TLSGD_ADD_LO12_NC => {
                if !gd_tail_is_call_nop(out, offset) {
                    return Err(ApplyError::BadInstruction);
                }
                patch(
                    out,
                    offset,
                    insn::ldr_offset(0, 0),
                    Field::LdSt {
                        scale: 3,
                        checked: false,
                    },
                    values.got as i64,
                )?;
                put(out, offset.wrapping_add(4), insn::MRS_TPIDR_X0 | 1)?;
                put(out, offset.wrapping_add(8), ADD_X0_X1_X0)
            }
            _ => Err(ApplyError::BadInstruction),
        },
        Kind::LdToLe => match r_type {
            // `tpoff` is the distance from the thread pointer to the start
            // of the module's TLS block, which local-dynamic code uses as
            // its base.
            R_AARCH64_TLSLD_ADR_PAGE21 => put(out, offset, insn::MRS_TPIDR_X0),
            R_AARCH64_TLSLD_ADD_LO12_NC => {
                patch(out, offset, insn::add_imm(0, 0), Field::Add12Checked, tpoff)?;
                put(out, offset.wrapping_add(4), NOP)
            }
            _ => Err(ApplyError::BadInstruction),
        },
        _ => Err(ApplyError::BadInstruction),
    }
}

/// Replaces a `bl`/`b` to an undefined weak symbol with a `nop`, as GNU ld
/// does: the symbol has no address, so the call is skipped rather than
/// branching to zero.
///
/// # Errors
///
/// [`ApplyError::OutOfBounds`] when the instruction is outside the section.
pub fn nop_undefined_branch(out: &mut [u8], offset: u64, r_type: u32) -> Result<bool, ApplyError> {
    if !matches!(r_type, R_AARCH64_CALL26 | R_AARCH64_JUMP26) {
        return Ok(false);
    }
    put(out, offset, NOP)?;
    Ok(true)
}

/// Fills `out` with `nop` instructions; a partial word is zeroed.
pub fn write_nops(out: &mut [u8]) {
    let (words, rest) = out.as_chunks_mut::<4>();
    for word in words {
        *word = NOP.to_le_bytes();
    }
    rest.fill(0);
}

/// `stp x16, x30, [sp, #-16]!`.
const STP_X16_X30: u32 = 0xa9bf_7bf0;

/// Writes the sequence that loads `.got.plt` slot `slot` into `x17` and
/// branches to it, at address `at` in `out` starting at `start`.
fn write_got_jump(
    out: &mut [u8],
    start: u64,
    at: u64,
    slot: u64,
    end: u64,
) -> Result<(), ApplyError> {
    let delta = (page(slot) as i64).wrapping_sub(page(at) as i64);
    patch(out, start, insn::adrp(16), Field::Adrp21, delta)?;
    patch(
        out,
        start.wrapping_add(4),
        insn::ldr_offset(17, 16),
        Field::LdSt {
            scale: 3,
            checked: false,
        },
        slot as i64,
    )?;
    patch(
        out,
        start.wrapping_add(8),
        insn::add_imm(16, 16),
        Field::Add12,
        slot as i64,
    )?;
    put(out, start.wrapping_add(12), insn::BR_X17)?;
    let mut pad = start.wrapping_add(16);
    while pad < end {
        put(out, pad, NOP)?;
        pad = pad.wrapping_add(4);
    }
    Ok(())
}

/// Writes the PLT header at address `plt`: save `x16`/`x30`, then jump to
/// the resolver through `.got.plt[2]`.
///
/// # Errors
///
/// [`ApplyError`] when `.got.plt` is more than 4 GiB from `.plt`.
pub fn write_plt_header(
    out: &mut [u8],
    plt: u64,
    got_plt: u64,
    flags: PltFlags,
) -> Result<(), ApplyError> {
    let mut at = 0u64;
    if flags.landing_pad {
        put(out, at, insn::BTI_C)?;
        at = at.wrapping_add(4);
    }
    put(out, at, STP_X16_X30)?;
    at = at.wrapping_add(4);
    let slot = got_plt.wrapping_add(16);
    write_got_jump(out, at, plt.wrapping_add(at), slot, 32)
}

/// Writes a `.plt`, `.plt.got` or IFUNC entry at address `entry` that jumps
/// through the GOT word at `slot`.
///
/// # Errors
///
/// [`ApplyError`] when the slot is more than 4 GiB from the entry.
pub fn write_plt_entry(
    out: &mut [u8],
    entry: u64,
    slot: u64,
    flags: PltFlags,
) -> Result<(), ApplyError> {
    let mut at = 0u64;
    if flags.entry_landing_pad {
        put(out, at, insn::BTI_C)?;
        at = at.wrapping_add(4);
    }
    let end = if flags.entry_landing_pad { 24 } else { 16 };
    write_got_jump(out, at, entry.wrapping_add(at), slot, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec() -> ClassifyContext {
        ClassifyContext::static_exec(true)
    }

    fn shared() -> ClassifyContext {
        ClassifyContext {
            relax_got: false,
            pic: true,
            tls: TlsMode::Dynamic,
            tls_ld: TlsMode::Dynamic,
            code: true,
        }
    }

    #[test]
    fn data_and_branch_types_are_classified() {
        assert_eq!(
            classify(R_AARCH64_ABS64, exec()).unwrap(),
            Class::new(Kind::Abs, Width::W64)
        );
        assert_eq!(
            classify(R_AARCH64_CALL26, exec()).unwrap().width,
            Width::Field(Field::Branch26)
        );
        assert_eq!(
            classify(R_AARCH64_ADR_PREL_PG_HI21, exec()).unwrap().kind,
            Kind::Page
        );
        assert_eq!(
            classify(R_AARCH64_LDST64_ABS_LO12_NC, exec())
                .unwrap()
                .width,
            Width::Field(Field::LdSt {
                scale: 3,
                checked: false
            })
        );
    }

    #[test]
    fn unsupported_types_are_rejected() {
        for r_type in [
            R_AARCH64_COPY,
            R_AARCH64_GLOB_DAT,
            R_AARCH64_JUMP_SLOT,
            R_AARCH64_RELATIVE,
            R_AARCH64_IRELATIVE,
            R_AARCH64_AUTH_ABS64,
            0xdead,
        ] {
            assert_eq!(
                classify(r_type, exec()),
                Err(ClassifyError::Unsupported),
                "type {r_type:#x}"
            );
        }
    }

    #[test]
    fn got_and_tls_use_the_right_slot() {
        let got = classify(R_AARCH64_ADR_GOT_PAGE, exec()).unwrap();
        assert_eq!(got.kind, Kind::GotPage);
        assert_eq!(got.slot, GotKind::Address);
        assert!(got.needs_got());
        let ie = classify(R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21, shared()).unwrap();
        assert_eq!(ie.slot, GotKind::TpOff);
        assert!(ie.needs_gottpoff());
        let gd = classify(R_AARCH64_TLSGD_ADR_PAGE21, shared()).unwrap();
        assert_eq!(gd.slot, GotKind::TlsGd);
        let desc = classify(R_AARCH64_TLSDESC_LD64_LO12, shared()).unwrap();
        assert_eq!(desc.slot, GotKind::TlsDesc);
        assert!(desc.is_tls());
    }

    #[test]
    fn executables_relax_tls() {
        assert_eq!(
            classify(R_AARCH64_TLSGD_ADR_PAGE21, exec()).unwrap().kind,
            Kind::GdToLe
        );
        let add = classify(R_AARCH64_TLSGD_ADD_LO12_NC, exec()).unwrap();
        assert_eq!(add.kind, Kind::GdToLe);
        assert!(add.skip_next);
        assert_eq!(
            classify(R_AARCH64_TLSDESC_CALL, exec()).unwrap().kind,
            Kind::DescCallToLe
        );
        assert_eq!(
            classify(R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21, exec())
                .unwrap()
                .kind,
            Kind::IeToLe
        );
    }

    /// The instruction sequences GNU ld 2.45 produces for the same inputs.
    #[test]
    fn tls_relaxation_matches_gnu_ld() {
        let values = RelaxValues {
            tpoff: 0x48,
            ..RelaxValues::default()
        };
        // adrp x0, :tlsgd:v; add x0, x0, :tlsgd_lo12:v; bl __tls_get_addr; nop
        let mut code = Vec::new();
        for word in [0x9000_0000u32, 0x9100_0000, 0x9400_0000, NOP] {
            code.extend_from_slice(&word.to_le_bytes());
        }
        relax_tls(
            &mut code,
            0,
            Kind::GdToLe,
            R_AARCH64_TLSGD_ADR_PAGE21,
            values,
        )
        .unwrap();
        relax_tls(
            &mut code,
            4,
            Kind::GdToLe,
            R_AARCH64_TLSGD_ADD_LO12_NC,
            values,
        )
        .unwrap();
        let words: Vec<u32> = code
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        assert_eq!(
            words,
            [0xd2a0_0000, 0xf280_0900, 0xd53b_d041, 0x8b00_0020],
            "movz x0, #0, lsl #16; movk x0, #0x48; mrs x1, tpidr_el0; add x0, x1, x0"
        );

        // adrp x0, :tlsdesc:v; ldr x1, [x0]; add x0, x0, #0; blr x1
        let mut code = Vec::new();
        for word in [0x9000_0000u32, 0xf940_0001, 0x9100_0000, 0xd63f_0020] {
            code.extend_from_slice(&word.to_le_bytes());
        }
        for (offset, r_type) in [
            (0, R_AARCH64_TLSDESC_ADR_PAGE21),
            (4, R_AARCH64_TLSDESC_LD64_LO12),
            (8, R_AARCH64_TLSDESC_ADD_LO12),
            (12, R_AARCH64_TLSDESC_CALL),
        ] {
            relax_tls(&mut code, offset, Kind::DescToLe, r_type, values).unwrap();
        }
        let words: Vec<u32> = code
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        assert_eq!(words, [0xd2a0_0000, 0xf280_0900, NOP, NOP]);

        // adrp x0, :gottprel:v; ldr x0, [x0, #:gottprel_lo12:v]
        let mut code = Vec::new();
        for word in [0x9000_0000u32, 0xf940_0000] {
            code.extend_from_slice(&word.to_le_bytes());
        }
        relax_tls(
            &mut code,
            0,
            Kind::IeToLe,
            R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21,
            values,
        )
        .unwrap();
        relax_tls(
            &mut code,
            4,
            Kind::IeToLe,
            R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC,
            values,
        )
        .unwrap();
        let words: Vec<u32> = code
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        assert_eq!(words, [0xd2a0_0000, 0xf280_0900]);
    }

    /// The PLT GNU ld 2.45 writes for a dynamic executable whose `.plt` is
    /// at 0x5f0 and `.got.plt` at 0x1ffe8.
    #[test]
    fn plt_matches_gnu_ld() {
        let mut header = [0u8; 32];
        write_plt_header(&mut header, 0x5f0, 0x1ffe8, PltFlags::default()).unwrap();
        let words: Vec<u32> = header
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        assert_eq!(
            words,
            [
                0xa9bf_7bf0, // stp x16, x30, [sp, #-16]!
                0xf000_00f0, // adrp x16, 1f000
                0xf947_fe11, // ldr x17, [x16, #4088]
                0x913f_e210, // add x16, x16, #0xff8
                0xd61f_0220, // br x17
                NOP,
                NOP,
                NOP,
            ]
        );
        let mut entry = [0u8; 16];
        write_plt_entry(&mut entry, 0x610, 0x20000, PltFlags::default()).unwrap();
        let words: Vec<u32> = entry
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        assert_eq!(words, [0x9000_0110, 0xf940_0211, 0x9100_0210, 0xd61f_0220]);
        // With BTI the entry grows to 24 bytes and starts with `bti c`.
        let mut entry = [0u8; 24];
        write_plt_entry(
            &mut entry,
            0x6a0,
            0x20000,
            PltFlags {
                landing_pad: true,
                entry_landing_pad: true,
            },
        )
        .unwrap();
        let words: Vec<u32> = entry
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        assert_eq!(
            words,
            [
                0xd503_245f,
                0x9000_0110,
                0xf940_0211,
                0x9100_0210,
                0xd61f_0220,
                NOP
            ]
        );
    }
}
