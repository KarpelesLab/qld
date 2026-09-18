//! RISC-V 64 (RV64, LP64/LP64D, little-endian) relocations.
//!
//! RISC-V relocations are not independent the way x86-64 and AArch64 ones
//! are, so besides the shared [`classify`] this backend has its own
//! relocation writer ([`apply`]) and a layout hook ([`relax`]):
//!
//! - A `%pcrel_lo` (`R_RISCV_PCREL_LO12_*`) names the *label* of its
//!   `auipc`, not the symbol: its value is the low part of whatever the
//!   `R_RISCV_*_HI20` at that label computes (a PC-relative address, a GOT
//!   slot, a TLS GOT entry). The TLS descriptor `LOAD_LO12`/`ADD_LO12`/`CALL`
//!   relocations likewise take their meaning from the `TLSDESC_HI20` before
//!   them.
//! - `R_RISCV_ADD*`/`SUB*`/`SET*` and `SET_ULEB128`/`SUB_ULEB128` compute
//!   label differences in place (DWARF, `.eh_frame`, jump tables); they are
//!   final at link time and never become dynamic relocations.
//! - Linker relaxation deletes bytes from code sections: calls become `jal`
//!   or `c.j`, local-exec TLS and small absolute addresses lose their `lui`,
//!   TLS descriptors become local-exec or initial-exec sequences, and
//!   `R_RISCV_ALIGN` padding is trimmed (always, even with `--no-relax`).
//!   See [`relax`] for how layout iterates and how symbols follow.
//!
//! The decisions follow lld (`lld/ELF/Arch/RISCV.cpp`), so both linkers
//! produce the same code: no global-pointer relaxation unless asked, no
//! general-dynamic or initial-exec TLS relaxation (the psABI defines none),
//! and descriptors relaxed only in executables.
//!
//! The PLT is the psABI's (and GNU ld's and lld's): a 32-byte header that
//! computes the `.got.plt` index from `t1` and jumps to the resolver, then
//! 16-byte entries `auipc t3; ld t3; jalr t1, t3; nop`.

#![deny(clippy::arithmetic_side_effects)]

pub mod apply;
pub mod attributes;
pub mod relax;

use crate::arch::riscv::{
    self as insn, AUIPC, Field, JALR, LD, NOP, SRLI, SUB, T0, T1, T2, T3, hi20, itype, lo12, rtype,
    utype,
};
use crate::elf::read::consts::riscv::*;

use super::{ApplyError, Class, ClassifyContext, ClassifyError, GotKind, Kind, TlsMode, Width};

/// `e_flags`: the object uses compressed instructions.
pub const EF_RISCV_RVC: u32 = 0x1;
/// `e_flags`: the floating-point ABI.
pub const EF_RISCV_FLOAT_ABI: u32 = 0x6;
/// `e_flags`: the RV32E/RV64E base.
pub const EF_RISCV_RVE: u32 = 0x8;
/// `p_type` of the segment that holds `.riscv.attributes`.
pub const PT_RISCV_ATTRIBUTES: u32 = 0x7000_0003;

/// lld's `INTERNAL_R_RISCV_*` numbers are above 255; qld does not need
/// them, since relaxation records its rewrites separately ([`relax`]).
const fn class(kind: Kind, field: Field) -> Class {
    Class::new(kind, Width::RiscV(field))
}

const fn got(kind: Kind, field: Field, slot: GotKind) -> Class {
    class(kind, field).through(slot)
}

const fn none() -> Class {
    Class::new(Kind::None, Width::None)
}

/// Whether `r_type` is an upper-20-bit relocation a `%pcrel_lo` may refer
/// to.
#[must_use]
pub const fn is_pcrel_hi(r_type: u32) -> bool {
    matches!(
        r_type,
        R_RISCV_PCREL_HI20 | R_RISCV_GOT_HI20 | R_RISCV_TLS_GD_HI20 | R_RISCV_TLS_GOT_HI20
    )
}

/// Classifies RISC-V relocation `r_type`.
///
/// Relocations whose value comes from another one (`%pcrel_lo`, the
/// descriptor tail) and the relaxation markers classify as [`Kind::None`]:
/// the scan has nothing to do for them, and [`apply`] handles them.
///
/// # Errors
///
/// [`ClassifyError::Unsupported`] for dynamic-only types, the deprecated
/// `RVC_LUI`/`GPREL_*`/`TPREL_I`/`TPREL_S` and unknown or vendor types.
pub fn classify(r_type: u32, context: ClassifyContext) -> Result<Class, ClassifyError> {
    use Field as F;
    use Kind as K;
    Ok(match r_type {
        R_RISCV_NONE | R_RISCV_RELAX | R_RISCV_ALIGN | R_RISCV_TPREL_ADD | R_RISCV_VENDOR => none(),
        R_RISCV_PCREL_LO12_I
        | R_RISCV_PCREL_LO12_S
        | R_RISCV_TLSDESC_LOAD_LO12
        | R_RISCV_TLSDESC_ADD_LO12
        | R_RISCV_TLSDESC_CALL => none(),

        R_RISCV_32 => class(K::Abs, F::Word32),
        R_RISCV_64 => Class::new(K::Abs, Width::W64),
        R_RISCV_HI20 => class(K::Abs, F::Hi20),
        R_RISCV_LO12_I => class(K::Abs, F::Lo12I),
        R_RISCV_LO12_S => class(K::Abs, F::Lo12S),

        R_RISCV_BRANCH => class(K::Pc, F::Branch),
        R_RISCV_JAL => class(K::Pc, F::Jal),
        R_RISCV_RVC_BRANCH => class(K::Pc, F::RvcBranch),
        R_RISCV_RVC_JUMP => class(K::Pc, F::RvcJump),
        R_RISCV_CALL | R_RISCV_CALL_PLT => class(K::Pc, F::Call),
        R_RISCV_PLT32 | R_RISCV_32_PCREL => class(K::Pc, F::Word32Signed),
        R_RISCV_PCREL_HI20 => class(K::Pc, F::Hi20),

        R_RISCV_GOT_HI20 => got(K::Got, F::Hi20, GotKind::Address),
        R_RISCV_GOT32_PCREL => got(K::Got, F::Word32Signed, GotKind::Address),

        R_RISCV_TPREL_HI20 => class(K::TpOff, F::Hi20),
        R_RISCV_TPREL_LO12_I => class(K::TpOff, F::Lo12I),
        R_RISCV_TPREL_LO12_S => class(K::TpOff, F::Lo12S),
        R_RISCV_TLS_GOT_HI20 => got(K::Got, F::Hi20, GotKind::TpOff),
        R_RISCV_TLS_GD_HI20 => got(K::Got, F::Hi20, GotKind::TlsGd),
        R_RISCV_TLSDESC_HI20 => match context.tls {
            TlsMode::Dynamic => got(K::Got, F::Hi20, GotKind::TlsDesc),
            TlsMode::LocalExec => Class::new(K::DescToLe, Width::None),
            TlsMode::InitialExec => Class::new(K::DescToIe, Width::None),
        },
        R_RISCV_TLS_DTPREL32 => class(K::DtpOff, F::Dtprel32),
        R_RISCV_TLS_DTPREL64 => class(K::DtpOff, F::Dtprel64),

        R_RISCV_ADD8 => Class::new(K::Add, Width::Any8),
        R_RISCV_ADD16 => Class::new(K::Add, Width::Any16),
        R_RISCV_ADD32 => Class::new(K::Add, Width::U32),
        R_RISCV_ADD64 => Class::new(K::Add, Width::W64),
        R_RISCV_SUB6 => class(K::Abs, F::Sub6),
        R_RISCV_SUB8 => Class::new(K::Sub, Width::Any8),
        R_RISCV_SUB16 => Class::new(K::Sub, Width::Any16),
        R_RISCV_SUB32 => Class::new(K::Sub, Width::U32),
        R_RISCV_SUB64 => Class::new(K::Sub, Width::W64),
        R_RISCV_SET6 => class(K::Abs, F::Set6),
        R_RISCV_SET8 => class(K::Abs, F::Set8),
        R_RISCV_SET16 => class(K::Abs, F::Set16),
        R_RISCV_SET32 => class(K::Abs, F::Set32),
        R_RISCV_SET_ULEB128 => class(K::Abs, F::SetUleb128),
        R_RISCV_SUB_ULEB128 => class(K::Abs, F::SubUleb128),

        _ => return Err(ClassifyError::Unsupported),
    })
}

/// Whether `r_type` is a call that goes through the PLT when its symbol is
/// preemptible.
#[must_use]
pub const fn is_branch(r_type: u32) -> bool {
    matches!(r_type, R_RISCV_CALL | R_RISCV_CALL_PLT | R_RISCV_PLT32)
}

fn put(out: &mut [u8], at: u64, value: u32) -> Result<(), ApplyError> {
    let at = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    insn::write32(out, at, value).ok_or(ApplyError::OutOfBounds)
}

/// Size of the PLT header.
pub const PLT_HEADER_SIZE: u64 = 32;
/// Size of one PLT entry (and of a static IFUNC stub).
pub const PLT_ENTRY_SIZE: u64 = 16;

/// Writes the PLT header at address `plt`:
///
/// ```text
/// 1: auipc t2, %pcrel_hi(.got.plt)
///    sub   t1, t1, t3               # t1 = &.plt[i] + 12 - &.plt[0] - 32 ...
///    ld    t3, %pcrel_lo(1b)(t2)    # _dl_runtime_resolve
///    addi  t1, t1, -(32 + 12)       # ... scaled below to the slot index
///    addi  t0, t2, %pcrel_lo(1b)    # &.got.plt
///    srli  t1, t1, 1                # .got.plt slot offset
///    ld    t0, 8(t0)                # link_map
///    jr    t3
/// ```
///
/// # Errors
///
/// [`ApplyError::Overflow`] when `.got.plt` is out of `auipc` range.
pub fn write_plt_header(out: &mut [u8], plt: u64, got_plt: u64) -> Result<(), ApplyError> {
    let offset = got_plt.wrapping_sub(plt);
    insn::check_hi(offset).map_err(|_| ApplyError::Overflow)?;
    let header_adjust = 0u32.wrapping_sub(PLT_HEADER_SIZE as u32).wrapping_sub(12);
    let words = [
        utype(AUIPC, T2, hi20(offset)),
        rtype(SUB, T1, T1, T3),
        itype(LD, T3, T2, lo12(offset)),
        itype(insn::ADDI, T1, T1, header_adjust),
        itype(insn::ADDI, T0, T2, lo12(offset)),
        itype(SRLI, T1, T1, 1),
        itype(LD, T0, T0, 8),
        itype(JALR, 0, T3, 0),
    ];
    for (index, word) in (0u64..).zip(words) {
        put(out, index.wrapping_mul(4), word)?;
    }
    Ok(())
}

/// Writes a PLT entry (or IFUNC stub) at address `entry` that jumps through
/// the GOT word at `slot`: `auipc t3, %pcrel_hi(slot); ld t3,
/// %pcrel_lo(slot)(t3); jalr t1, t3; nop`.
///
/// # Errors
///
/// [`ApplyError::Overflow`] when the slot is out of `auipc` range.
pub fn write_plt_entry(out: &mut [u8], entry: u64, slot: u64) -> Result<(), ApplyError> {
    let offset = slot.wrapping_sub(entry);
    insn::check_hi(offset).map_err(|_| ApplyError::Overflow)?;
    put(out, 0, utype(AUIPC, T3, hi20(offset)))?;
    put(out, 4, itype(LD, T3, T3, lo12(offset)))?;
    put(out, 8, itype(JALR, T1, T3, 0))?;
    put(out, 12, NOP)
}

/// Fills the gaps of code sections as lld does: with zeros, which decode as
/// an illegal instruction.
pub fn write_nops(out: &mut [u8]) {
    out.fill(0);
}

/// The `e_flags` of the output: the first object's, with `RVC` if any
/// object uses compressed instructions (lld's rule).
#[must_use]
pub fn output_flags(flags: impl IntoIterator<Item = u32>) -> u32 {
    let mut flags = flags.into_iter();
    let Some(first) = flags.next() else {
        return 0;
    };
    flags.fold(first, |merged, f| merged | (f & EF_RISCV_RVC))
}

/// Why an object cannot be linked with the first one, if it cannot: a
/// different floating-point ABI or base (RV64E).
#[must_use]
pub fn incompatible_flags(first: u32, flags: u32) -> Option<&'static str> {
    if flags & EF_RISCV_FLOAT_ABI != first & EF_RISCV_FLOAT_ABI {
        return Some("floating-point ABI");
    }
    if flags & EF_RISCV_RVE != first & EF_RISCV_RVE {
        return Some("EF_RISCV_RVE");
    }
    None
}

/// The program interpreter for the floating-point ABI in `e_flags`.
#[must_use]
pub fn interpreter(flags: u32) -> &'static str {
    match flags & EF_RISCV_FLOAT_ABI {
        0 => "/lib/ld-linux-riscv64-lp64.so.1",
        2 => "/lib/ld-linux-riscv64-lp64f.so.1",
        _ => "/lib/ld-linux-riscv64-lp64d.so.1",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec() -> ClassifyContext {
        ClassifyContext::static_exec(true)
    }

    fn words(bytes: &[u8]) -> Vec<u32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect()
    }

    #[test]
    fn relocations_are_classified() {
        assert_eq!(
            classify(R_RISCV_64, exec()).unwrap(),
            Class::new(Kind::Abs, Width::W64)
        );
        assert_eq!(
            classify(R_RISCV_CALL_PLT, exec()).unwrap().width,
            Width::RiscV(Field::Call)
        );
        let got = classify(R_RISCV_GOT_HI20, exec()).unwrap();
        assert!(got.needs_got());
        let ie = classify(R_RISCV_TLS_GOT_HI20, exec()).unwrap();
        assert!(ie.needs_gottpoff());
        assert_eq!(
            classify(R_RISCV_PCREL_LO12_I, exec()).unwrap().kind,
            Kind::None
        );
        assert_eq!(
            classify(R_RISCV_TLSDESC_HI20, exec()).unwrap().kind,
            Kind::DescToLe
        );
        for r_type in [R_RISCV_RVC_LUI, R_RISCV_GPREL_I, R_RISCV_RELATIVE, 200] {
            assert_eq!(
                classify(r_type, exec()),
                Err(ClassifyError::Unsupported),
                "{r_type}"
            );
        }
    }

    /// The PLT lld 23 writes for a shared object whose `.plt` is at 0x1310
    /// and `.got.plt` at 0x3400.
    #[test]
    fn plt_matches_lld() {
        let mut header = [0u8; 32];
        write_plt_header(&mut header, 0x1310, 0x3400).unwrap();
        assert_eq!(
            words(&header),
            [
                0x0000_2397, // auipc t2, 0x2
                0x41c3_0333, // sub t1, t1, t3
                0x0f03_be03, // ld t3, 0xf0(t2)
                0xfd43_0313, // addi t1, t1, -0x2c
                0x0f03_8293, // addi t0, t2, 0xf0
                0x0013_5313, // srli t1, t1, 0x1
                0x0082_b283, // ld t0, 0x8(t0)
                0x000e_0067, // jr t3
            ]
        );
        let mut entry = [0u8; 16];
        write_plt_entry(&mut entry, 0x1330, 0x3410).unwrap();
        assert_eq!(words(&entry), [0x0000_2e17, 0x0e0e_3e03, 0x000e_0367, NOP]);
    }

    #[test]
    fn flags_merge_rvc_and_reject_abi_mismatches() {
        assert_eq!(output_flags([0x4, 0x5, 0x4]), 0x5);
        assert_eq!(output_flags([]), 0);
        assert_eq!(incompatible_flags(0x5, 0x1), Some("floating-point ABI"));
        assert_eq!(incompatible_flags(0x5, 0x4), None);
        assert_eq!(interpreter(0x5), "/lib/ld-linux-riscv64-lp64d.so.1");
    }
}
