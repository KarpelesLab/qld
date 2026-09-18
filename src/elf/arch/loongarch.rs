//! LoongArch64 (LP64, little-endian) relocations: classification, TLS and
//! GOT relaxation, linker relaxation without shrinking, and the PLT.
//!
//! [`classify`] turns a relocation type into a [`Class`], as for the other
//! architectures. Most of the psABI maps onto the shared kinds: a
//! `pcalau12i` relocation is a [`Kind::Page`] (whose page delta the
//! architecture computes, [`page_delta`]), the `addi`/`ld`/`st` that
//! completes it a [`Kind::PageOff`], the DWARF and `.eh_frame` label
//! differences [`Kind::Add`]/[`Kind::Sub`], and the instruction fields
//! [`Field`]s of [`crate::arch::loongarch`].
//!
//! **What is rewritten**, following lld (the reference for these rules):
//!
//! - *GOT to PC-relative:* `pcalau12i rd, %got_pc_hi20(s); ld.d rd, rd,
//!   %got_pc_lo12(s)` against a symbol that is defined here, not
//!   preemptible and not an IFUNC becomes `pcalau12i; addi.d` computing its
//!   address. qld checks the pair by its instructions, since relocations
//!   are classified one at a time: both halves must be adjacent and use one
//!   register, and a pair that is not is left alone (lld also relaxes pairs
//!   the compiler scheduled apart). Unlike lld, qld then drops the unused
//!   GOT entry.
//! - *TLS:* initial-exec accesses to a variable of the executable become
//!   local-exec (`lu12i.w`/`ori`, or `nop`/`ori` when the offset fits 12
//!   bits), again for adjacent pairs only, and descriptors become
//!   local-exec or initial-exec; the `pcalau12i`/`addi.d` that computed the
//!   descriptor's address become `nop`s. General- and local-dynamic accesses
//!   are kept, as lld keeps them (LoongArch has no transition for them);
//!   local-dynamic uses the variable's own module/offset pair, as in lld and
//!   mold.
//! - *Linker relaxation* (`R_LARCH_RELAX` follows the relocation): the
//!   rewrites that do not change the code size. `pcalau12i` + `addi.d` (or
//!   a relaxable GOT load) within ±2 MiB becomes `nop` + `pcaddi`,
//!   `pcaddu18i` + `jirl` within ±128 MiB becomes `bl` (or `b`) + `nop`,
//!   and a local-exec `lu12i.w`/`add.d`/`addi.d` whose offset fits 12 bits
//!   becomes `nop`/`nop`/`addi.d rd, $tp, off`. Deleting the `nop`s, and the
//!   excess padding an `R_LARCH_ALIGN` marks, needs section shrinking,
//!   which qld does not do yet: the padding stays in place as `nop`s, so
//!   the code is correct but the aligned label is not moved onto its
//!   boundary.
//!
//! Whether `R_LARCH_RELAX` follows is not part of the relocation, so the
//! relocation loops mark it in the type ([`RELAX_HINT`], through
//! [`super::Arch::annotate`]) before classifying.
//!
//! The PLT is lld's and GNU ld's: a 32-byte header that computes the
//! `.got.plt` index from `$t1`/`$t3` and jumps to the resolver, then one
//! 16-byte `pcaddu12i`/`ld.d`/`jirl`/`nop` entry per symbol. `.got.plt`
//! reserves two words, for the resolver and the link map.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::loongarch::{
    self as insn, ADDI_D, ADDI_W, Field, LD_D, LD_W, LU12I_W, NOP, ORI, PCADDI, PCADDU12I,
    PCALAU12I, R_A0, R_T0, R_T1, R_T2, R_T3, R_TP, R_ZERO, SRLI_D, SUB_D, read_insn, write_insn,
};

use super::{
    ApplyError, Class, ClassifyContext, ClassifyError, GotKind, Kind, RelaxValues, TlsMode, Width,
};

/// Declares relocation type constants and a name lookup function.
macro_rules! relocation_types {
    ($($name:ident = $value:expr,)*) => {
        $(
            #[doc = concat!("Relocation type `", stringify!($name), "`.")]
            pub const $name: u32 = $value;
        )*

        /// The name of LoongArch relocation type `r_type`, as `readelf`
        /// prints it.
        #[must_use]
        pub fn reloc_name(r_type: u32) -> Option<&'static str> {
            match r_type & !RELAX_HINT {
                $($name => Some(stringify!($name)),)*
                _ => None,
            }
        }
    };
}

relocation_types! {
    R_LARCH_NONE = 0,
    R_LARCH_32 = 1,
    R_LARCH_64 = 2,
    R_LARCH_RELATIVE = 3,
    R_LARCH_COPY = 4,
    R_LARCH_JUMP_SLOT = 5,
    R_LARCH_TLS_DTPMOD32 = 6,
    R_LARCH_TLS_DTPMOD64 = 7,
    R_LARCH_TLS_DTPREL32 = 8,
    R_LARCH_TLS_DTPREL64 = 9,
    R_LARCH_TLS_TPREL32 = 10,
    R_LARCH_TLS_TPREL64 = 11,
    R_LARCH_IRELATIVE = 12,
    R_LARCH_TLS_DESC32 = 13,
    R_LARCH_TLS_DESC64 = 14,
    R_LARCH_MARK_LA = 20,
    R_LARCH_MARK_PCREL = 21,
    R_LARCH_SOP_PUSH_PCREL = 22,
    R_LARCH_SOP_PUSH_ABSOLUTE = 23,
    R_LARCH_SOP_PUSH_DUP = 24,
    R_LARCH_SOP_PUSH_GPREL = 25,
    R_LARCH_SOP_PUSH_TLS_TPREL = 26,
    R_LARCH_SOP_PUSH_TLS_GOT = 27,
    R_LARCH_SOP_PUSH_TLS_GD = 28,
    R_LARCH_SOP_PUSH_PLT_PCREL = 29,
    R_LARCH_SOP_ASSERT = 30,
    R_LARCH_SOP_NOT = 31,
    R_LARCH_SOP_SUB = 32,
    R_LARCH_SOP_SL = 33,
    R_LARCH_SOP_SR = 34,
    R_LARCH_SOP_ADD = 35,
    R_LARCH_SOP_AND = 36,
    R_LARCH_SOP_IF_ELSE = 37,
    R_LARCH_SOP_POP_32_S_10_5 = 38,
    R_LARCH_SOP_POP_32_U_10_12 = 39,
    R_LARCH_SOP_POP_32_S_10_12 = 40,
    R_LARCH_SOP_POP_32_S_10_16 = 41,
    R_LARCH_SOP_POP_32_S_10_16_S2 = 42,
    R_LARCH_SOP_POP_32_S_5_20 = 43,
    R_LARCH_SOP_POP_32_S_0_5_10_16_S2 = 44,
    R_LARCH_SOP_POP_32_S_0_10_10_16_S2 = 45,
    R_LARCH_SOP_POP_32_U = 46,
    R_LARCH_ADD8 = 47,
    R_LARCH_ADD16 = 48,
    R_LARCH_ADD24 = 49,
    R_LARCH_ADD32 = 50,
    R_LARCH_ADD64 = 51,
    R_LARCH_SUB8 = 52,
    R_LARCH_SUB16 = 53,
    R_LARCH_SUB24 = 54,
    R_LARCH_SUB32 = 55,
    R_LARCH_SUB64 = 56,
    R_LARCH_GNU_VTINHERIT = 57,
    R_LARCH_GNU_VTENTRY = 58,
    R_LARCH_B16 = 64,
    R_LARCH_B21 = 65,
    R_LARCH_B26 = 66,
    R_LARCH_ABS_HI20 = 67,
    R_LARCH_ABS_LO12 = 68,
    R_LARCH_ABS64_LO20 = 69,
    R_LARCH_ABS64_HI12 = 70,
    R_LARCH_PCALA_HI20 = 71,
    R_LARCH_PCALA_LO12 = 72,
    R_LARCH_PCALA64_LO20 = 73,
    R_LARCH_PCALA64_HI12 = 74,
    R_LARCH_GOT_PC_HI20 = 75,
    R_LARCH_GOT_PC_LO12 = 76,
    R_LARCH_GOT64_PC_LO20 = 77,
    R_LARCH_GOT64_PC_HI12 = 78,
    R_LARCH_GOT_HI20 = 79,
    R_LARCH_GOT_LO12 = 80,
    R_LARCH_GOT64_LO20 = 81,
    R_LARCH_GOT64_HI12 = 82,
    R_LARCH_TLS_LE_HI20 = 83,
    R_LARCH_TLS_LE_LO12 = 84,
    R_LARCH_TLS_LE64_LO20 = 85,
    R_LARCH_TLS_LE64_HI12 = 86,
    R_LARCH_TLS_IE_PC_HI20 = 87,
    R_LARCH_TLS_IE_PC_LO12 = 88,
    R_LARCH_TLS_IE64_PC_LO20 = 89,
    R_LARCH_TLS_IE64_PC_HI12 = 90,
    R_LARCH_TLS_IE_HI20 = 91,
    R_LARCH_TLS_IE_LO12 = 92,
    R_LARCH_TLS_IE64_LO20 = 93,
    R_LARCH_TLS_IE64_HI12 = 94,
    R_LARCH_TLS_LD_PC_HI20 = 95,
    R_LARCH_TLS_LD_HI20 = 96,
    R_LARCH_TLS_GD_PC_HI20 = 97,
    R_LARCH_TLS_GD_HI20 = 98,
    R_LARCH_32_PCREL = 99,
    R_LARCH_RELAX = 100,
    R_LARCH_ALIGN = 102,
    R_LARCH_PCREL20_S2 = 103,
    R_LARCH_ADD6 = 105,
    R_LARCH_SUB6 = 106,
    R_LARCH_ADD_ULEB128 = 107,
    R_LARCH_SUB_ULEB128 = 108,
    R_LARCH_64_PCREL = 109,
    R_LARCH_CALL36 = 110,
    R_LARCH_TLS_DESC_PC_HI20 = 111,
    R_LARCH_TLS_DESC_PC_LO12 = 112,
    R_LARCH_TLS_DESC64_PC_LO20 = 113,
    R_LARCH_TLS_DESC64_PC_HI12 = 114,
    R_LARCH_TLS_DESC_HI20 = 115,
    R_LARCH_TLS_DESC_LO12 = 116,
    R_LARCH_TLS_DESC64_LO20 = 117,
    R_LARCH_TLS_DESC64_HI12 = 118,
    R_LARCH_TLS_DESC_LD = 119,
    R_LARCH_TLS_DESC_CALL = 120,
    R_LARCH_TLS_LE_HI20_R = 121,
    R_LARCH_TLS_LE_ADD_R = 122,
    R_LARCH_TLS_LE_LO12_R = 123,
    R_LARCH_TLS_LD_PCREL20_S2 = 124,
    R_LARCH_TLS_GD_PCREL20_S2 = 125,
    R_LARCH_TLS_DESC_PCREL20_S2 = 126,
    R_LARCH_CALL30 = 127,
    R_LARCH_PCADD_HI20 = 128,
    R_LARCH_PCADD_LO12 = 129,
    R_LARCH_GOT_PCADD_HI20 = 130,
    R_LARCH_GOT_PCADD_LO12 = 131,
    R_LARCH_TLS_IE_PCADD_HI20 = 132,
    R_LARCH_TLS_IE_PCADD_LO12 = 133,
    R_LARCH_TLS_LD_PCADD_HI20 = 134,
    R_LARCH_TLS_LD_PCADD_LO12 = 135,
    R_LARCH_TLS_GD_PCADD_HI20 = 136,
    R_LARCH_TLS_GD_PCADD_LO12 = 137,
    R_LARCH_TLS_DESC_PCADD_HI20 = 138,
    R_LARCH_TLS_DESC_PCADD_LO12 = 139,
}

/// Set in a relocation type by [`super::Arch::annotate`] when the next
/// relocation is an `R_LARCH_RELAX` at the same offset, which allows the
/// linker to rewrite the instructions. No LoongArch type uses this bit.
pub const RELAX_HINT: u32 = 1 << 31;

/// The ABI modifier bits of `e_flags` (soft, single or double float).
pub const EF_LOONGARCH_ABI_MODIFIER_MASK: u32 = 0x07;
/// The LP64D ABI modifier: 64-bit pointers, double-precision float
/// registers.
pub const EF_LOONGARCH_ABI_DOUBLE_FLOAT: u32 = 0x03;
/// The object ABI version bits of `e_flags`.
pub const EF_LOONGARCH_OBJABI_MASK: u32 = 0xc0;
/// Object ABI v1: the psABI 2.x relocations, without the stack machine.
pub const EF_LOONGARCH_OBJABI_V1: u32 = 0x40;

/// The type without [`RELAX_HINT`].
#[must_use]
pub const fn base_type(r_type: u32) -> u32 {
    r_type & !RELAX_HINT
}

const fn class(kind: Kind, width: Width) -> Class {
    Class::new(kind, width)
}

const fn got(kind: Kind, width: Width, slot: GotKind) -> Class {
    Class::new(kind, width).through(slot)
}

const fn field(f: Field) -> Width {
    Width::LoongArch(f)
}

fn insn_at(data: &[u8], offset: u64) -> Option<u32> {
    read_insn(data, usize::try_from(offset).ok()?)
}

/// The adjacent `pcalau12i rd` + `op rd, rd, …` pair starting at
/// `offset`, if there is one and the second instruction satisfies `second`.
fn pair_at(data: &[u8], offset: u64, second: fn(u32) -> bool) -> bool {
    let (Some(hi), Some(lo)) = (
        insn_at(data, offset),
        offset.checked_add(4).and_then(|o| insn_at(data, o)),
    ) else {
        return false;
    };
    insn::is_pcalau12i(hi)
        && second(lo)
        && insn::rd(hi) == insn::rj(lo)
        && insn::rj(lo) == insn::rd(lo)
}

/// The same pair, seen from its second instruction at `offset`.
fn pair_before(data: &[u8], offset: u64, second: fn(u32) -> bool) -> bool {
    offset
        .checked_sub(4)
        .is_some_and(|start| pair_at(data, start, second))
}

/// Classifies LoongArch relocation `r_type` (which may carry
/// [`RELAX_HINT`]) at `offset` in section `data`.
///
/// # Errors
///
/// [`ClassifyError::Unsupported`] for types qld does not link: the
/// dynamic-only types, the stack-machine relocations of object ABI v0, the
/// 24-bit label arithmetic lld also rejects, and the LA32 `pcaddu12i`
/// forms.
#[allow(clippy::too_many_lines)]
pub fn classify(
    r_type: u32,
    addend: i64,
    data: &[u8],
    offset: u64,
    context: ClassifyContext,
) -> Result<Class, ClassifyError> {
    use Field as F;
    use Kind as K;
    use Width as W;
    let hint = r_type & RELAX_HINT != 0 && context.relax_got;
    let tls = context.tls;
    // A GOT load of a symbol resolved here, as an adjacent pair the GOT
    // indirection can be taken out of.
    let got_relaxable = context.relax_got && addend == 0;
    Ok(match base_type(r_type) {
        R_LARCH_NONE
        | R_LARCH_MARK_LA
        | R_LARCH_MARK_PCREL
        | R_LARCH_GNU_VTINHERIT
        | R_LARCH_GNU_VTENTRY
        | R_LARCH_RELAX
        | R_LARCH_ALIGN => class(K::None, W::None),

        // Data.
        R_LARCH_64 => class(K::Abs, W::W64),
        R_LARCH_32 => class(K::Abs, W::Any32),
        R_LARCH_64_PCREL => class(K::Pc, W::W64),
        R_LARCH_32_PCREL => class(K::Pc, W::I32),
        R_LARCH_TLS_DTPREL64 => class(K::DtpOff, W::W64),
        R_LARCH_TLS_DTPREL32 => class(K::DtpOff, W::Any32),
        R_LARCH_TLS_TPREL64 => class(K::TpOff, W::W64),
        R_LARCH_TLS_TPREL32 => class(K::TpOff, W::Any32),

        // Label differences (DWARF, `.eh_frame`, jump tables).
        R_LARCH_ADD6 => class(K::Add, field(F::Data6)),
        R_LARCH_ADD8 => class(K::Add, W::Any8),
        R_LARCH_ADD16 => class(K::Add, W::Any16),
        R_LARCH_ADD32 => class(K::Add, W::Any32),
        R_LARCH_ADD64 => class(K::Add, W::W64),
        R_LARCH_ADD_ULEB128 => class(K::Add, field(F::Uleb128)),
        R_LARCH_SUB6 => class(K::Sub, field(F::Data6)),
        R_LARCH_SUB8 => class(K::Sub, W::Any8),
        R_LARCH_SUB16 => class(K::Sub, W::Any16),
        R_LARCH_SUB32 => class(K::Sub, W::Any32),
        R_LARCH_SUB64 => class(K::Sub, W::W64),
        R_LARCH_SUB_ULEB128 => class(K::Sub, field(F::Uleb128)),

        // Branches.
        R_LARCH_B16 => class(K::Pc, field(F::B16)),
        R_LARCH_B21 => class(K::Pc, field(F::B21)),
        R_LARCH_B26 => class(K::Pc, field(F::B26)),
        R_LARCH_CALL36 if r_type & RELAX_HINT != 0 && context.relax_got => {
            class(K::Pc, field(F::Call36Relax))
        }
        R_LARCH_CALL36 => class(K::Pc, field(F::Call36)),
        R_LARCH_PCREL20_S2 => class(K::Pc, field(F::Pcrel20S2)),

        // Absolute addresses.
        R_LARCH_ABS_HI20 => class(K::Abs, field(F::Hi20)),
        R_LARCH_ABS_LO12 => class(K::Abs, field(F::Lo12)),
        R_LARCH_ABS64_LO20 => class(K::Abs, field(F::Lo20)),
        R_LARCH_ABS64_HI12 => class(K::Abs, field(F::Hi12)),

        // PC-relative addresses.
        R_LARCH_PCALA_HI20 if hint && pair_at(data, offset, insn::is_addi) => {
            class(K::Relax, W::None)
        }
        R_LARCH_PCALA_HI20 => class(K::Page, field(F::Hi20)),
        R_LARCH_PCALA_LO12 => match insn_at(data, offset) {
            // `pcalau12i` + `jirl`: a call through the low part.
            Some(word) if insn::is_jirl(word) => class(K::PageOff, field(F::JirlLo12)),
            _ if hint && pair_before(data, offset, insn::is_addi) => class(K::Relax, W::None),
            _ => class(K::PageOff, field(F::Lo12)),
        },
        R_LARCH_PCALA64_LO20 => class(K::Page, field(F::Lo20)),
        R_LARCH_PCALA64_HI12 => class(K::Page, field(F::Hi12)),

        // GOT.
        R_LARCH_GOT_PC_HI20 if got_relaxable && pair_at(data, offset, insn::is_ld_word) => {
            class(K::Relax, W::None)
        }
        R_LARCH_GOT_PC_HI20 => got(K::GotPage, field(F::Hi20), GotKind::Address),
        R_LARCH_GOT_PC_LO12 => match insn_at(data, offset) {
            Some(word) if insn::is_ld_word(word) => {
                if got_relaxable && pair_before(data, offset, insn::is_ld_word) {
                    class(K::Relax, W::None)
                } else {
                    got(K::GotAbs, field(F::Lo12), GotKind::Address)
                }
            }
            // An `addi.d` computes the address of the entry rather than
            // loading it: the second half of a general- or local-dynamic
            // sequence, which reuses this type. (The extreme code model's
            // `addi.d rd, $zero` is ambiguous and taken as a GOT load.)
            Some(word) if insn::is_addi(word) && insn::rj(word) != R_ZERO => {
                got(K::GotAbs, field(F::Lo12), GotKind::TlsGd)
            }
            _ => got(K::GotAbs, field(F::Lo12), GotKind::Address),
        },
        R_LARCH_GOT64_PC_LO20 => got(K::GotPage, field(F::Lo20), GotKind::Address),
        R_LARCH_GOT64_PC_HI12 => got(K::GotPage, field(F::Hi12), GotKind::Address),
        R_LARCH_GOT_HI20 => got(K::GotAbs, field(F::Hi20), GotKind::Address),
        R_LARCH_GOT_LO12 => got(K::GotAbs, field(F::Lo12), GotKind::Address),
        R_LARCH_GOT64_LO20 => got(K::GotAbs, field(F::Lo20), GotKind::Address),
        R_LARCH_GOT64_HI12 => got(K::GotAbs, field(F::Hi12), GotKind::Address),

        // Local-exec.
        R_LARCH_TLS_LE_HI20 => class(K::TpOff, field(F::Hi20)),
        R_LARCH_TLS_LE_LO12 => class(K::TpOff, field(F::Lo12)),
        R_LARCH_TLS_LE64_LO20 => class(K::TpOff, field(F::Lo20)),
        R_LARCH_TLS_LE64_HI12 => class(K::TpOff, field(F::Hi12)),
        R_LARCH_TLS_LE_HI20_R if hint => class(K::TpOff, field(F::TpHi20Relax)),
        R_LARCH_TLS_LE_HI20_R => class(K::TpOff, field(F::Hi20Round)),
        R_LARCH_TLS_LE_ADD_R if hint => class(K::TpOff, field(F::TpAddRelax)),
        R_LARCH_TLS_LE_ADD_R => class(K::None, W::None),
        R_LARCH_TLS_LE_LO12_R if hint => class(K::TpOff, field(F::TpLo12Relax)),
        R_LARCH_TLS_LE_LO12_R => class(K::TpOff, field(F::Lo12)),

        // Initial-exec, relaxed to local-exec for adjacent pairs.
        R_LARCH_TLS_IE_PC_HI20
            if tls == TlsMode::LocalExec && pair_at(data, offset, insn::is_ld_word) =>
        {
            class(K::IeToLe, W::None)
        }
        R_LARCH_TLS_IE_PC_HI20 => got(K::GotPage, field(F::Hi20), GotKind::TpOff),
        R_LARCH_TLS_IE_PC_LO12
            if tls == TlsMode::LocalExec
                && insn_at(data, offset).is_some_and(insn::is_ld_word)
                && pair_before(data, offset, insn::is_ld_word) =>
        {
            class(K::IeToLe, W::None)
        }
        R_LARCH_TLS_IE_PC_LO12 => got(K::GotAbs, field(F::Lo12), GotKind::TpOff),
        R_LARCH_TLS_IE64_PC_LO20 => got(K::GotPage, field(F::Lo20), GotKind::TpOff),
        R_LARCH_TLS_IE64_PC_HI12 => got(K::GotPage, field(F::Hi12), GotKind::TpOff),
        R_LARCH_TLS_IE_HI20 => got(K::GotAbs, field(F::Hi20), GotKind::TpOff),
        R_LARCH_TLS_IE_LO12 => got(K::GotAbs, field(F::Lo12), GotKind::TpOff),
        R_LARCH_TLS_IE64_LO20 => got(K::GotAbs, field(F::Lo20), GotKind::TpOff),
        R_LARCH_TLS_IE64_HI12 => got(K::GotAbs, field(F::Hi12), GotKind::TpOff),

        // General- and local-dynamic: never relaxed.
        R_LARCH_TLS_GD_PC_HI20 | R_LARCH_TLS_LD_PC_HI20 => {
            got(K::GotPage, field(F::Hi20), GotKind::TlsGd)
        }
        R_LARCH_TLS_GD_HI20 => got(K::GotAbs, field(F::Hi20), GotKind::TlsGd),
        R_LARCH_TLS_LD_HI20 => got(K::GotAbs, field(F::Hi20), GotKind::TlsLd),
        R_LARCH_TLS_GD_PCREL20_S2 => got(K::Got, field(F::Pcrel20S2), GotKind::TlsGd),
        R_LARCH_TLS_LD_PCREL20_S2 => got(K::Got, field(F::Pcrel20S2), GotKind::TlsLd),

        // TLS descriptors.
        R_LARCH_TLS_DESC_PC_HI20 => desc(tls, got(K::GotPage, field(F::Hi20), GotKind::TlsDesc)),
        R_LARCH_TLS_DESC_PC_LO12 => desc(tls, got(K::GotAbs, field(F::Lo12), GotKind::TlsDesc)),
        R_LARCH_TLS_DESC_PCREL20_S2 => {
            desc(tls, got(K::Got, field(F::Pcrel20S2), GotKind::TlsDesc))
        }
        R_LARCH_TLS_DESC_LD | R_LARCH_TLS_DESC_CALL => desc(tls, class(K::None, W::None)),
        // The extreme code model's upper halves and the absolute forms
        // compute the descriptor's address, which a relaxed `DESC_LD`
        // overwrites: left alone.
        R_LARCH_TLS_DESC64_PC_LO20 => {
            desc_dead(tls, got(K::GotPage, field(F::Lo20), GotKind::TlsDesc))
        }
        R_LARCH_TLS_DESC64_PC_HI12 => {
            desc_dead(tls, got(K::GotPage, field(F::Hi12), GotKind::TlsDesc))
        }
        R_LARCH_TLS_DESC_HI20 => desc_dead(tls, got(K::GotAbs, field(F::Hi20), GotKind::TlsDesc)),
        R_LARCH_TLS_DESC_LO12 => desc_dead(tls, got(K::GotAbs, field(F::Lo12), GotKind::TlsDesc)),
        R_LARCH_TLS_DESC64_LO20 => desc_dead(tls, got(K::GotAbs, field(F::Lo20), GotKind::TlsDesc)),
        R_LARCH_TLS_DESC64_HI12 => desc_dead(tls, got(K::GotAbs, field(F::Hi12), GotKind::TlsDesc)),

        _ => return Err(ClassifyError::Unsupported),
    })
}

/// A TLS descriptor relocation: `dynamic` in a shared object, rewritten
/// otherwise.
const fn desc(tls: TlsMode, dynamic: Class) -> Class {
    match tls {
        TlsMode::Dynamic => dynamic,
        TlsMode::LocalExec => class(Kind::DescToLe, Width::None),
        TlsMode::InitialExec => class(Kind::DescToIe, Width::None),
    }
}

/// A descriptor relocation whose instruction is dead once the sequence is
/// relaxed.
const fn desc_dead(tls: TlsMode, dynamic: Class) -> Class {
    match tls {
        TlsMode::Dynamic => dynamic,
        _ => class(Kind::None, Width::None),
    }
}

/// Whether relocation `r_type` is a branch that goes through the PLT when
/// its symbol is preemptible.
#[must_use]
pub fn is_branch(r_type: u32) -> bool {
    matches!(
        base_type(r_type),
        R_LARCH_B16 | R_LARCH_B21 | R_LARCH_B26 | R_LARCH_CALL36
    )
}

/// The page delta a `pcalau12i` relocation (or, in the extreme code
/// model, the `lu32i.d`/`lu52i.d` after it) computes from `place` to
/// `target`.
#[must_use]
pub fn page_delta(target: u64, place: u64, r_type: u32) -> u64 {
    // The later instructions of `pcalau12i; addi.d; lu32i.d; lu52i.d`
    // compute relative to the `pcalau12i`.
    let pc = match base_type(r_type) {
        R_LARCH_PCALA64_LO20
        | R_LARCH_GOT64_PC_LO20
        | R_LARCH_TLS_IE64_PC_LO20
        | R_LARCH_TLS_DESC64_PC_LO20 => place.wrapping_sub(8),
        R_LARCH_PCALA64_HI12
        | R_LARCH_GOT64_PC_HI12
        | R_LARCH_TLS_IE64_PC_HI12
        | R_LARCH_TLS_DESC64_PC_HI12 => place.wrapping_sub(12),
        _ => place,
    };
    insn::page_delta(target, pc)
}

fn word(out: &[u8], at: u64) -> Result<u32, ApplyError> {
    insn_at(out, at).ok_or(ApplyError::OutOfBounds)
}

fn put(out: &mut [u8], at: u64, value: u32) -> Result<(), ApplyError> {
    let at = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    write_insn(out, at, value).ok_or(ApplyError::OutOfBounds)
}

fn patch(out: &mut [u8], at: u64, field: Field, value: i64) -> Result<(), ApplyError> {
    let insn = word(out, at)?;
    let encoded = field
        .encode(insn, value)
        .map_err(|_| ApplyError::Overflow)?;
    put(out, at, encoded)
}

/// Writes `value` into `field` at `offset`: the fields that are not one
/// instruction word, and the relaxable forms, which pick their encoding
/// from the value.
///
/// # Errors
///
/// [`ApplyError::Overflow`] or [`ApplyError::OutOfBounds`].
pub fn write_field(
    out: &mut [u8],
    offset: u64,
    field: Field,
    value: u64,
) -> Result<(), ApplyError> {
    let signed = value as i64;
    let next = offset.checked_add(4).ok_or(ApplyError::OutOfBounds)?;
    match field {
        Field::Call36 | Field::Call36Relax => {
            let (first, second) = (word(out, offset)?, word(out, next)?);
            // `pcaddu18i` + `jirl rd, …` becomes `bl`/`b` + `nop` when the
            // target is in reach and the call links `$ra` or nothing.
            let link = insn::rd(second);
            if field == Field::Call36Relax
                && (link == insn::R_RA || link == R_ZERO)
                && Field::B26.encode(0, signed).is_ok()
            {
                let op = if link == insn::R_RA {
                    insn::BL
                } else {
                    insn::B
                };
                patch_word(out, offset, op, Field::B26, signed)?;
                return put(out, next, NOP);
            }
            let [first, second] =
                insn::call36(first, second, signed).map_err(|_| ApplyError::Overflow)?;
            put(out, offset, first)?;
            put(out, next, second)
        }
        Field::TpHi20Relax => {
            if insn::fits_signed(signed, 12) {
                put(out, offset, NOP)
            } else {
                patch(out, offset, Field::Hi20Round, signed)
            }
        }
        Field::TpAddRelax => {
            if insn::fits_signed(signed, 12) {
                put(out, offset, NOP)
            } else {
                Ok(())
            }
        }
        Field::TpLo12Relax => {
            let insn = word(out, offset)?;
            let insn = if insn::fits_signed(signed, 12) {
                insn::with_rj(insn, R_TP)
            } else {
                insn
            };
            let encoded = Field::Lo12
                .encode(insn, signed)
                .map_err(|_| ApplyError::Overflow)?;
            put(out, offset, encoded)
        }
        Field::Data6 => {
            let at = usize::try_from(offset).map_err(|_| ApplyError::OutOfBounds)?;
            let byte = out.get_mut(at).ok_or(ApplyError::OutOfBounds)?;
            *byte = (*byte & 0xc0) | (value as u8 & 0x3f);
            Ok(())
        }
        Field::Uleb128 => {
            let at = usize::try_from(offset).map_err(|_| ApplyError::OutOfBounds)?;
            let rest = out.get_mut(at..).ok_or(ApplyError::OutOfBounds)?;
            let (_, length) = insn::read_uleb128(rest).ok_or(ApplyError::OutOfBounds)?;
            let bytes = rest.get_mut(..length).ok_or(ApplyError::OutOfBounds)?;
            insn::write_uleb128(bytes, value);
            Ok(())
        }
        _ => patch(out, offset, field, signed),
    }
}

fn patch_word(
    out: &mut [u8],
    at: u64,
    insn: u32,
    field: Field,
    value: i64,
) -> Result<(), ApplyError> {
    let encoded = field
        .encode(insn, value)
        .map_err(|_| ApplyError::Overflow)?;
    put(out, at, encoded)
}

/// Adds `delta` to the data field at `offset` (`R_LARCH_ADD6` and
/// `R_LARCH_ADD_ULEB128`, or their `SUB` counterparts with a negated
/// delta).
///
/// # Errors
///
/// [`ApplyError::OutOfBounds`], also for a malformed ULEB128 number.
pub fn add_field(out: &mut [u8], offset: u64, field: Field, delta: u64) -> Result<(), ApplyError> {
    let at = usize::try_from(offset).map_err(|_| ApplyError::OutOfBounds)?;
    match field {
        Field::Data6 => {
            let byte = out.get_mut(at).ok_or(ApplyError::OutOfBounds)?;
            *byte = (*byte & 0xc0) | (byte.wrapping_add(delta as u8) & 0x3f);
            Ok(())
        }
        Field::Uleb128 => {
            let rest = out.get_mut(at..).ok_or(ApplyError::OutOfBounds)?;
            let (value, length) = insn::read_uleb128(rest).ok_or(ApplyError::OutOfBounds)?;
            let bytes = rest.get_mut(..length).ok_or(ApplyError::OutOfBounds)?;
            insn::write_uleb128(bytes, value.wrapping_add(delta));
            Ok(())
        }
        _ => Err(ApplyError::BadInstruction),
    }
}

/// Whether a `pcaddi` placed `delta` bytes before its target, and one
/// placed four bytes later, both reach it: the test both halves of a
/// relaxed pair apply, so that they agree.
fn pcaddi_reaches(delta: i64) -> bool {
    delta & 3 == 0 && insn::fits_signed(delta, 22) && insn::fits_signed(delta.wrapping_sub(4), 22)
}

/// Rewrites one half of a `pcalau12i` pair classified [`Kind::Relax`]:
/// the address of `target` computed without the GOT, by a `pcaddi` when
/// relaxation is allowed and it reaches.
///
/// # Errors
///
/// [`ApplyError`] when the instructions are not the pair classified, or
/// the target is out of reach.
pub fn relax(
    out: &mut [u8],
    offset: u64,
    r_type: u32,
    target: u64,
    place: u64,
) -> Result<(), ApplyError> {
    let pcaddi = r_type & RELAX_HINT != 0;
    match base_type(r_type) {
        R_LARCH_PCALA_HI20 | R_LARCH_GOT_PC_HI20 => {
            let delta = target.wrapping_sub(place) as i64;
            if pcaddi && pcaddi_reaches(delta) {
                return put(out, offset, NOP);
            }
            let insn = word(out, offset)?;
            let insn = insn::ri20(PCALAU12I, insn::rd(insn), 0);
            patch_word(
                out,
                offset,
                insn,
                Field::Hi20,
                page_delta(target, place, r_type) as i64,
            )
        }
        R_LARCH_PCALA_LO12 | R_LARCH_GOT_PC_LO12 => {
            let lo = word(out, offset)?;
            let delta = target.wrapping_sub(place.wrapping_sub(4)) as i64;
            if pcaddi && pcaddi_reaches(delta) {
                let insn = insn::ri20(PCADDI, insn::rd(lo), 0);
                return patch_word(
                    out,
                    offset,
                    insn,
                    Field::Pcrel20S2,
                    target.wrapping_sub(place) as i64,
                );
            }
            let op = if lo & 0xffc0_0000 == LD_W || lo & 0xffc0_0000 == ADDI_W {
                ADDI_W
            } else {
                ADDI_D
            };
            let insn = insn::rri12(op, insn::rd(lo), insn::rj(lo), 0);
            patch_word(out, offset, insn, Field::Lo12, target as i64)
        }
        _ => Err(ApplyError::BadInstruction),
    }
}

/// Rewrites one instruction of a relaxed TLS sequence.
///
/// # Errors
///
/// [`ApplyError`] for relocations that are not part of a relaxable
/// sequence, and thread pointer offsets beyond ±2 GiB.
pub fn relax_tls(
    out: &mut [u8],
    offset: u64,
    kind: Kind,
    r_type: u32,
    values: RelaxValues,
) -> Result<(), ApplyError> {
    let tpoff = values.tpoff;
    let small = insn::fits_unsigned(tpoff, 12);
    let hi20 = |rd: u32| insn::ri20(LU12I_W, rd, ((tpoff as u64) >> 12) as u32);
    let lo12 = |rd: u32, rj: u32| insn::rri12(ORI, rd, rj, (tpoff as u64 & 0xfff) as u32);
    match (kind, base_type(r_type)) {
        (Kind::IeToLe, R_LARCH_TLS_IE_PC_HI20) => {
            if !insn::fits_signed(tpoff, 32) {
                return Err(ApplyError::Overflow);
            }
            let rd = insn::rd(word(out, offset)?);
            put(out, offset, if small { NOP } else { hi20(rd) })
        }
        (Kind::IeToLe, R_LARCH_TLS_IE_PC_LO12) => {
            if !insn::fits_signed(tpoff, 32) {
                return Err(ApplyError::Overflow);
            }
            let current = word(out, offset)?;
            let rd = insn::rd(current);
            let source = if small { R_ZERO } else { insn::rj(current) };
            put(out, offset, lo12(rd, source))
        }
        (
            Kind::DescToLe | Kind::DescToIe,
            R_LARCH_TLS_DESC_PC_HI20 | R_LARCH_TLS_DESC_PC_LO12 | R_LARCH_TLS_DESC_PCREL20_S2,
        ) => put(out, offset, NOP),
        (Kind::DescToLe, R_LARCH_TLS_DESC_LD) => {
            if !insn::fits_signed(tpoff, 32) {
                return Err(ApplyError::Overflow);
            }
            put(out, offset, if small { NOP } else { hi20(R_A0) })
        }
        (Kind::DescToLe, R_LARCH_TLS_DESC_CALL) => {
            if !insn::fits_signed(tpoff, 32) {
                return Err(ApplyError::Overflow);
            }
            put(out, offset, lo12(R_A0, if small { R_ZERO } else { R_A0 }))
        }
        (Kind::DescToIe, R_LARCH_TLS_DESC_LD) => patch_word(
            out,
            offset,
            insn::ri20(PCALAU12I, R_A0, 0),
            Field::Hi20,
            insn::page_delta(values.got, values.place) as i64,
        ),
        (Kind::DescToIe, R_LARCH_TLS_DESC_CALL) => patch_word(
            out,
            offset,
            insn::rri12(LD_D, R_A0, R_A0, 0),
            Field::Lo12,
            values.got as i64,
        ),
        _ => Err(ApplyError::BadInstruction),
    }
}

/// Makes a `b`/`bl` or conditional branch to an undefined weak symbol, which
/// has no address, branch to itself, as lld does on RISC-V: address 0 is out
/// of reach of an executable at GNU ld's base, and the call is guarded
/// anyway (`if (f) f();`). A `pcaddu18i` + `jirl` pair reaches 0 and keeps
/// its target.
///
/// # Errors
///
/// [`ApplyError::OutOfBounds`] when the instruction is outside the section.
pub fn undefined_weak_branch(out: &mut [u8], offset: u64, r_type: u32) -> Result<bool, ApplyError> {
    let field = match base_type(r_type) {
        R_LARCH_B16 => Field::B16,
        R_LARCH_B21 => Field::B21,
        R_LARCH_B26 => Field::B26,
        _ => return Ok(false),
    };
    patch(out, offset, field, 0)?;
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

/// Size of the PLT header.
pub const PLT_HEADER_SIZE: u64 = 32;
/// Size of a PLT entry (and of an IFUNC stub).
pub const PLT_ENTRY_SIZE: u64 = 16;

/// Writes the PLT header at address `plt`:
///
/// ```text
/// pcaddu12i $t2, %pcrel_hi20(.got.plt)
/// sub.d     $t1, $t1, $t3
/// ld.d      $t3, $t2, %pcrel_lo12(.got.plt)  # _dl_runtime_resolve
/// addi.d    $t1, $t1, -44                    # &.plt[i] - &.plt[0]
/// addi.d    $t0, $t2, %pcrel_lo12(.got.plt)
/// srli.d    $t1, $t1, 1                      # &.got.plt[i] - &.got.plt[2]
/// ld.d      $t0, $t0, 8                      # link map
/// jr        $t3
/// ```
///
/// # Errors
///
/// [`ApplyError::OutOfBounds`] when `out` is shorter than the header.
pub fn write_plt_header(out: &mut [u8], plt: u64, got_plt: u64) -> Result<(), ApplyError> {
    let offset = got_plt.wrapping_sub(plt) as u32;
    let lo = offset & 0xfff;
    let back = (PLT_HEADER_SIZE as u32).wrapping_add(12).wrapping_neg() & 0xfff;
    let words = [
        insn::ri20(PCADDU12I, R_T2, insn::pcrel_hi20(offset)),
        insn::rrr(SUB_D, R_T1, R_T1, R_T3),
        insn::rri12(LD_D, R_T3, R_T2, lo),
        insn::rri12(ADDI_D, R_T1, R_T1, back),
        insn::rri12(ADDI_D, R_T0, R_T2, lo),
        insn::rri12(SRLI_D, R_T1, R_T1, 1),
        insn::rri12(LD_D, R_T0, R_T0, 8),
        insn::jirl(R_ZERO, R_T3),
    ];
    write_words(out, &words)
}

/// Writes a PLT entry (or an IFUNC stub) at address `entry` that jumps
/// through the GOT word at `slot`:
///
/// ```text
/// pcaddu12i $t3, %pcrel_hi20(slot)
/// ld.d      $t3, $t3, %pcrel_lo12(slot)
/// jirl      $t1, $t3, 0
/// nop
/// ```
///
/// # Errors
///
/// [`ApplyError::OutOfBounds`] when `out` is shorter than an entry.
pub fn write_plt_entry(out: &mut [u8], entry: u64, slot: u64) -> Result<(), ApplyError> {
    let offset = slot.wrapping_sub(entry) as u32;
    let words = [
        insn::ri20(PCADDU12I, R_T3, insn::pcrel_hi20(offset)),
        insn::rri12(LD_D, R_T3, R_T3, offset & 0xfff),
        insn::jirl(R_T1, R_T3),
        NOP,
    ];
    write_words(out, &words)
}

fn write_words(out: &mut [u8], words: &[u32]) -> Result<(), ApplyError> {
    let (slots, _) = out.as_chunks_mut::<4>();
    if slots.len() < words.len() {
        return Err(ApplyError::OutOfBounds);
    }
    for (slot, word) in slots.iter_mut().zip(words) {
        *slot = word.to_le_bytes();
    }
    Ok(())
}

/// The `e_flags` of the output from each input object's flags and whether
/// it has code: those of the first object with code and non-zero flags, as
/// lld chooses them (objects without code, such as `objcopy`'d data, often
/// have none). Without one, LP64D with object ABI v1.
#[must_use]
pub fn output_flags(objects: impl Iterator<Item = (u32, bool)>) -> u32 {
    objects
        .filter(|&(flags, code)| code && flags != 0)
        .map(|(flags, _)| flags)
        .next()
        .unwrap_or(EF_LOONGARCH_OBJABI_V1 | EF_LOONGARCH_ABI_DOUBLE_FLOAT)
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

    fn code(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn words(bytes: &[u8]) -> Vec<u32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect()
    }

    /// `pcalau12i $a0, 0; ld.d $a0, $a0, 0`.
    const GOT_LOAD: [u32; 2] = [0x1a00_0004, 0x28c0_0084];
    /// `pcalau12i $a0, 0; addi.d $a0, $a0, 0`.
    const PCALA: [u32; 2] = [0x1a00_0004, 0x02c0_0084];

    #[test]
    fn names_round_trip() {
        assert_eq!(reloc_name(R_LARCH_CALL36), Some("R_LARCH_CALL36"));
        assert_eq!(
            reloc_name(R_LARCH_PCALA_HI20 | RELAX_HINT),
            Some("R_LARCH_PCALA_HI20")
        );
        assert_eq!(reloc_name(101), None);
    }

    #[test]
    fn data_and_branch_types_are_classified() {
        let empty: &[u8] = &[];
        let c = |t| classify(t, 0, empty, 0, exec()).unwrap();
        assert_eq!(c(R_LARCH_64), Class::new(Kind::Abs, Width::W64));
        assert_eq!(c(R_LARCH_B26).width, Width::LoongArch(Field::B26));
        assert_eq!(c(R_LARCH_CALL36).width, Width::LoongArch(Field::Call36));
        assert_eq!(
            c(R_LARCH_CALL36 | RELAX_HINT).width,
            Width::LoongArch(Field::Call36Relax)
        );
        assert_eq!(c(R_LARCH_ADD32).kind, Kind::Add);
        assert_eq!(c(R_LARCH_SUB_ULEB128).kind, Kind::Sub);
        assert_eq!(c(R_LARCH_RELAX).kind, Kind::None);
        assert_eq!(c(R_LARCH_ALIGN).kind, Kind::None);
        assert_eq!(c(R_LARCH_PCALA_HI20).kind, Kind::Page);
        assert_eq!(c(R_LARCH_PCALA_LO12).kind, Kind::PageOff);
        for r_type in [
            R_LARCH_RELATIVE,
            R_LARCH_JUMP_SLOT,
            R_LARCH_TLS_DESC64,
            R_LARCH_SOP_PUSH_PCREL,
            R_LARCH_ADD24,
            0xdead,
        ] {
            assert_eq!(
                classify(r_type, 0, empty, 0, exec()),
                Err(ClassifyError::Unsupported),
                "type {r_type}"
            );
        }
    }

    #[test]
    fn got_loads_relax_only_as_adjacent_pairs() {
        let data = code(&GOT_LOAD);
        let hi = classify(R_LARCH_GOT_PC_HI20, 0, &data, 0, exec()).unwrap();
        let lo = classify(R_LARCH_GOT_PC_LO12, 0, &data, 4, exec()).unwrap();
        assert_eq!((hi.kind, lo.kind), (Kind::Relax, Kind::Relax));
        // Not for a preemptible symbol, a non-zero addend or other code.
        let hi = classify(R_LARCH_GOT_PC_HI20, 0, &data, 0, shared()).unwrap();
        assert_eq!((hi.kind, hi.slot), (Kind::GotPage, GotKind::Address));
        let lo = classify(R_LARCH_GOT_PC_LO12, 8, &data, 4, exec()).unwrap();
        assert_eq!(lo.kind, Kind::GotAbs);
        let apart = code(&[GOT_LOAD[0], NOP, GOT_LOAD[1]]);
        assert_eq!(
            classify(R_LARCH_GOT_PC_HI20, 0, &apart, 0, exec())
                .unwrap()
                .kind,
            Kind::GotPage
        );
        assert_eq!(
            classify(R_LARCH_GOT_PC_LO12, 0, &apart, 8, exec())
                .unwrap()
                .kind,
            Kind::GotAbs
        );
        // The second half of a general-dynamic sequence is an `addi.d`.
        let gd = code(&PCALA);
        let lo = classify(R_LARCH_GOT_PC_LO12, 0, &gd, 4, shared()).unwrap();
        assert_eq!((lo.kind, lo.slot), (Kind::GotAbs, GotKind::TlsGd));
    }

    #[test]
    fn got_relaxation_rewrites_the_load() {
        // pcalau12i at 0x1_2000_0000, target 0x1_2000_5808.
        let mut data = code(&GOT_LOAD);
        let (place, target) = (0x1_2000_0000u64, 0x1_2000_5808u64);
        relax(&mut data, 0, R_LARCH_GOT_PC_HI20, target, place).unwrap();
        relax(&mut data, 4, R_LARCH_GOT_PC_LO12, target, place + 4).unwrap();
        // pcalau12i $a0, 6; addi.d $a0, $a0, -2040
        assert_eq!(
            words(&data),
            [0x1a00_00c4, insn::rri12(ADDI_D, R_A0, R_A0, 0x808)]
        );
        // With R_LARCH_RELAX: nop; pcaddi $a0, (target - place - 4) / 4.
        let mut data = code(&GOT_LOAD);
        relax(
            &mut data,
            0,
            R_LARCH_GOT_PC_HI20 | RELAX_HINT,
            target,
            place,
        )
        .unwrap();
        relax(
            &mut data,
            4,
            R_LARCH_GOT_PC_LO12 | RELAX_HINT,
            target,
            place + 4,
        )
        .unwrap();
        assert_eq!(
            words(&data),
            [NOP, insn::ri20(PCADDI, R_A0, ((0x5808 - 4) / 4) as u32)]
        );
        // Out of pcaddi's reach, the pair stays a pcalau12i + addi.d.
        let mut data = code(&PCALA);
        let far = place + (4 << 20);
        relax(&mut data, 0, R_LARCH_PCALA_HI20 | RELAX_HINT, far, place).unwrap();
        relax(
            &mut data,
            4,
            R_LARCH_PCALA_LO12 | RELAX_HINT,
            far,
            place + 4,
        )
        .unwrap();
        assert_eq!(words(&data), [0x1a00_8004, PCALA[1]]);
    }

    #[test]
    fn calls_relax_to_bl() {
        // pcaddu18i $ra, 0; jirl $ra, $ra, 0
        let call = [0x1e00_0001, 0x4c00_0021];
        let mut data = code(&call);
        write_field(&mut data, 0, Field::Call36Relax, 0x100).unwrap();
        assert_eq!(words(&data), [0x5401_0000, NOP]);
        // A tail call (`jr $t8`) becomes `b`.
        let mut data = code(&[0x1e00_0014, 0x4c00_0280]);
        write_field(&mut data, 0, Field::Call36Relax, 0x100).unwrap();
        assert_eq!(words(&data), [0x5001_0000, NOP]);
        // Beyond ±128 MiB it stays a pcaddu18i + jirl.
        let mut data = code(&call);
        write_field(&mut data, 0, Field::Call36Relax, 1 << 28).unwrap();
        assert_eq!(words(&data)[0], 0x1e00_0001 | (0x400 << 5));
        assert_eq!(words(&data)[1], call[1]);
    }

    /// The sequences lld 23 writes for the same inputs.
    #[test]
    fn tls_relaxation_matches_lld() {
        let values = |tpoff| RelaxValues {
            tpoff,
            ..RelaxValues::default()
        };
        // pcalau12i $a0, %ie_pc_hi20; ld.d $a0, $a0, %ie_pc_lo12
        for (tpoff, expect) in [
            (0x10, [NOP, insn::rri12(ORI, R_A0, R_ZERO, 0x10)]),
            (
                0x1_2345,
                [
                    insn::ri20(LU12I_W, R_A0, 0x12),
                    insn::rri12(ORI, R_A0, R_A0, 0x345),
                ],
            ),
        ] {
            let mut data = code(&GOT_LOAD);
            relax_tls(
                &mut data,
                0,
                Kind::IeToLe,
                R_LARCH_TLS_IE_PC_HI20,
                values(tpoff),
            )
            .unwrap();
            relax_tls(
                &mut data,
                4,
                Kind::IeToLe,
                R_LARCH_TLS_IE_PC_LO12,
                values(tpoff),
            )
            .unwrap();
            assert_eq!(words(&data), expect);
        }
        // pcalau12i $a0; addi.d $a0, $a0; ld.d $ra, $a0, 0; jirl $ra, $ra, 0
        let desc = [0x1a00_0004, 0x02c0_0084, 0x28c0_0081, 0x4c00_0021];
        let mut data = code(&desc);
        for (offset, r_type) in [
            (0, R_LARCH_TLS_DESC_PC_HI20),
            (4, R_LARCH_TLS_DESC_PC_LO12),
            (8, R_LARCH_TLS_DESC_LD),
            (12, R_LARCH_TLS_DESC_CALL),
        ] {
            relax_tls(&mut data, offset, Kind::DescToLe, r_type, values(0x20)).unwrap();
        }
        assert_eq!(
            words(&data),
            [NOP, NOP, NOP, insn::rri12(ORI, R_A0, R_ZERO, 0x20)]
        );
        // To initial-exec: the GOT entry at 0x1_2000_4010, code at 0x1_2000_0000.
        let mut data = code(&desc);
        let ie = RelaxValues {
            got: 0x1_2000_4010,
            place: 0x1_2000_0008,
            ..RelaxValues::default()
        };
        relax_tls(&mut data, 8, Kind::DescToIe, R_LARCH_TLS_DESC_LD, ie).unwrap();
        relax_tls(&mut data, 12, Kind::DescToIe, R_LARCH_TLS_DESC_CALL, ie).unwrap();
        assert_eq!(
            &words(&data)[2..],
            [
                insn::ri20(PCALAU12I, R_A0, 4),
                insn::rri12(LD_D, R_A0, R_A0, 0x10)
            ]
        );
    }

    #[test]
    fn local_exec_relaxes_to_the_thread_pointer() {
        // lu12i.w $a0, 0; add.d $a0, $a0, $tp; addi.d $a0, $a0, 0
        let seq = [0x1400_0004, 0x0010_8884, 0x02c0_0084];
        let mut data = code(&seq);
        write_field(&mut data, 0, Field::TpHi20Relax, 0x40).unwrap();
        write_field(&mut data, 4, Field::TpAddRelax, 0x40).unwrap();
        write_field(&mut data, 8, Field::TpLo12Relax, 0x40).unwrap();
        assert_eq!(
            words(&data),
            [NOP, NOP, insn::rri12(ADDI_D, R_A0, R_TP, 0x40)]
        );
        let mut data = code(&seq);
        write_field(&mut data, 0, Field::TpHi20Relax, 0x1800).unwrap();
        write_field(&mut data, 4, Field::TpAddRelax, 0x1800).unwrap();
        write_field(&mut data, 8, Field::TpLo12Relax, 0x1800).unwrap();
        assert_eq!(
            words(&data),
            [
                insn::ri20(LU12I_W, R_A0, 2),
                seq[1],
                insn::rri12(ADDI_D, R_A0, R_A0, 0x800)
            ]
        );
    }

    #[test]
    fn label_differences_add_in_place() {
        let mut data = vec![0xc5, 0x80, 0x01];
        add_field(&mut data, 0, Field::Data6, 3).unwrap();
        assert_eq!(data[0], 0xc8);
        add_field(&mut data, 0, Field::Data6, 0u64.wrapping_sub(9)).unwrap();
        assert_eq!(data[0], 0xff);
        add_field(&mut data, 1, Field::Uleb128, 0x10).unwrap();
        assert_eq!(&data[1..], [0x90, 0x01]);
        assert_eq!(
            add_field(&mut data, 3, Field::Uleb128, 1),
            Err(ApplyError::OutOfBounds)
        );
    }

    /// The PLT lld writes for `.plt` at 0x1_2000_0400 and `.got.plt` at
    /// 0x1_2000_8000.
    #[test]
    fn plt_matches_lld() {
        let mut header = [0u8; 32];
        write_plt_header(&mut header, 0x1_2000_0400, 0x1_2000_8000).unwrap();
        assert_eq!(
            words(&header),
            [
                0x1c00_010e, // pcaddu12i $t2, 8
                0x0011_bdad, // sub.d $t1, $t1, $t3
                0x28f0_01cf, // ld.d $t3, $t2, -1024
                0x02ff_51ad, // addi.d $t1, $t1, -44
                0x02f0_01cc, // addi.d $t0, $t2, -1024
                0x0045_05ad, // srli.d $t1, $t1, 1
                0x28c0_218c, // ld.d $t0, $t0, 8
                0x4c00_01e0, // jr $t3
            ]
        );
        let mut entry = [0u8; 16];
        write_plt_entry(&mut entry, 0x1_2000_0420, 0x1_2000_8010).unwrap();
        assert_eq!(words(&entry), [0x1c00_010f, 0x28ef_c1ef, 0x4c00_01ed, NOP]);
    }
}
