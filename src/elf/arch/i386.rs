//! i386 relocations: classification, relaxation, application, and the PLT
//! entry encodings.
//!
//! i386 objects use `SHT_REL`: a relocation's addend is the value stored in
//! the field it patches ([`implicit_addend`]), and the dynamic relocations
//! of the output are `SHT_REL` too, with their addends written into the
//! words they relocate.
//!
//! **The GOT base** is `_GLOBAL_OFFSET_TABLE_`, the start of `.got.plt`.
//! Position-independent code keeps it in `%ebx`; `R_386_GOTOFF` and
//! `R_386_GOT32` are relative to it, and `R_386_GOTPC` computes it.
//!
//! **Relaxations** follow GNU ld and lld:
//!
//! - `R_386_GOT32X` to a symbol defined in the output: `mov foo@GOT(%reg)`
//!   becomes `lea foo@GOTOFF(%reg)`, `call *foo@GOT(%reg)` becomes
//!   `addr32 call foo` and `jmp *foo@GOT(%reg)` becomes `jmp foo; nop`; in
//!   position-dependent output, `mov foo@GOT, %reg` (no base register)
//!   becomes `mov $foo, %reg`;
//! - TLS, GNU dialect ([`TlsMode`]): general-dynamic, local-dynamic,
//!   initial-exec and descriptors become local-exec in an executable that
//!   defines the variable, and general-dynamic and descriptors become
//!   initial-exec for a shared library's variable. The Sun dialect
//!   (`R_386_TLS_*_32` other than `LDO_32`) is not linked.
//!
//! **The PLT** of an executable addresses `.got.plt` absolutely; that of a
//! PIE or shared object through `%ebx`, as the ABI requires. Lazy entries
//! push the byte offset of their `R_386_JUMP_SLOT` in `.rel.plt`.

#![deny(clippy::arithmetic_side_effects)]

use crate::elf::read::consts::i386::*;

pub use super::{
    ApplyError, Class, ClassifyContext, ClassifyError, GotKind, Kind, RelaxValues, TlsMode, Width,
};

const fn class(kind: Kind, width: Width) -> Class {
    Class::new(kind, width)
}

const fn got(kind: Kind, width: Width, slot: GotKind) -> Class {
    Class::new(kind, width).through(slot)
}

/// Reads byte `offset - back` of `data`, if it exists.
fn byte_before(data: &[u8], offset: u64, back: u64) -> Option<u8> {
    let at = usize::try_from(offset.checked_sub(back)?).ok()?;
    data.get(at).copied()
}

/// Reads byte `offset + forward` of `data`, if it exists.
fn byte_after(data: &[u8], offset: u64, forward: u64) -> Option<u8> {
    let at = usize::try_from(offset.checked_add(forward)?).ok()?;
    data.get(at).copied()
}

/// The size of the field relocation `r_type` patches, in bytes.
#[must_use]
pub fn field_size(r_type: u32) -> usize {
    match r_type {
        R_386_16 | R_386_PC16 => 2,
        R_386_8 | R_386_PC8 => 1,
        R_386_NONE | R_386_TLS_DESC_CALL | R_386_GNU_VTINHERIT | R_386_GNU_VTENTRY => 0,
        _ => 4,
    }
}

/// The addend of relocation `r_type` at `offset` in section `data`: the
/// field's contents, sign-extended.
#[must_use]
pub fn implicit_addend(r_type: u32, data: &[u8], offset: u64) -> i64 {
    let Ok(at) = usize::try_from(offset) else {
        return 0;
    };
    let field = |n: usize| data.get(at..at.checked_add(n)?);
    match field_size(r_type) {
        4 => field(4)
            .and_then(|b| b.first_chunk::<4>())
            .map_or(0, |b| i64::from(i32::from_le_bytes(*b))),
        2 => field(2)
            .and_then(|b| b.first_chunk::<2>())
            .map_or(0, |b| i64::from(i16::from_le_bytes(*b))),
        1 => field(1)
            .and_then(|b| b.first())
            .map_or(0, |&b| i64::from(b as i8)),
        _ => 0,
    }
}

/// Whether the ModRM byte before `offset` addresses memory through a base
/// register, as `foo@GOT(%reg)`, rather than an absolute `disp32`.
fn has_base(data: &[u8], offset: u64) -> bool {
    byte_before(data, offset, 1).is_some_and(|modrm| modrm & 0xc7 != 0x05)
}

/// Classifies relocation `r_type` at `offset` in section `data`.
///
/// # Errors
///
/// [`ClassifyError`] for unsupported types and unrecognized TLS code.
pub fn classify(
    r_type: u32,
    data: &[u8],
    offset: u64,
    context: ClassifyContext,
) -> Result<Class, ClassifyError> {
    use Kind as K;
    use Width as W;
    Ok(match r_type {
        R_386_NONE | R_386_GNU_VTINHERIT | R_386_GNU_VTENTRY => class(K::None, W::None),
        R_386_32 => class(K::Abs, W::Any32),
        R_386_16 => class(K::Abs, W::Any16),
        R_386_8 => class(K::Abs, W::Any8),
        R_386_PC32 | R_386_PLT32 => class(K::Pc, W::Any32),
        R_386_PC16 => class(K::Pc, W::I16),
        R_386_PC8 => class(K::Pc, W::I8),
        R_386_GOTOFF => class(K::GotRel, W::Any32),
        R_386_GOTPC => class(K::GotBasePc, W::Any32),
        R_386_SIZE32 => class(K::Size, W::U32),
        R_386_GOT32 => {
            if has_base(data, offset) {
                class(K::GotSlotRel, W::Any32)
            } else {
                class(K::GotAbs, W::Any32)
            }
        }
        R_386_GOT32X => {
            let op = byte_before(data, offset, 2);
            let modrm = byte_before(data, offset, 1);
            let base = has_base(data, offset);
            let relax = context.relax_got && context.code;
            match (op, modrm) {
                // mov foo@GOT(%reg), %reg2 -> lea foo@GOTOFF(%reg), %reg2
                (Some(0x8b), _) if relax && base => class(K::RelaxGotOff, W::Any32),
                // mov foo@GOT, %reg -> mov $foo, %reg (no base register:
                // only position-dependent code reads the GOT this way)
                (Some(0x8b), _) if relax && !context.pic => class(K::RelaxGotPcNoPic, W::Any32),
                // call/jmp *foo@GOT(%reg) -> addr32 call foo / jmp foo; nop
                (Some(0xff), Some(m)) if relax && matches!(m & 0x38, 0x10 | 0x20) => {
                    class(K::RelaxGotPc, W::Any32)
                }
                _ if base => class(K::GotSlotRel, W::Any32),
                _ => class(K::GotAbs, W::Any32),
            }
        }
        R_386_TLS_LE => class(K::TpOff, W::Any32),
        R_386_TLS_LDO_32 => class(K::DtpOff, W::Any32),
        R_386_TLS_IE => match context.tls {
            TlsMode::LocalExec => class(K::IeToLe, W::None),
            _ => got(K::GotAbs, W::Any32, GotKind::TpOff),
        },
        R_386_TLS_GOTIE => match context.tls {
            TlsMode::LocalExec => class(K::IeToLe, W::None),
            _ => got(K::GotSlotRel, W::Any32, GotKind::TpOff),
        },
        R_386_TLS_GD => {
            if context.tls == TlsMode::Dynamic {
                return Ok(got(K::GotSlotRel, W::Any32, GotKind::TlsGd));
            }
            // leal x@tlsgd(,%ebx,1), %eax; call ___tls_get_addr@plt, or
            // leal x@tlsgd(%reg), %eax; call *___tls_get_addr@GOT(%reg).
            let sib = byte_before(data, offset, 2) == Some(0x04);
            let call = if sib {
                byte_after(data, offset, 4) == Some(0xe8)
            } else {
                byte_after(data, offset, 4) == Some(0xff)
            };
            if byte_before(data, offset, if sib { 3 } else { 2 }) != Some(0x8d) || !call {
                return Err(ClassifyError::BadTlsInstruction);
            }
            if context.tls == TlsMode::LocalExec {
                class(K::GdToLe, W::None).skipping()
            } else {
                class(K::GdToIe, W::None).skipping()
            }
        }
        R_386_TLS_LDM => {
            if context.tls_ld != TlsMode::LocalExec {
                return Ok(got(K::GotSlotRel, W::Any32, GotKind::TlsLd));
            }
            // leal x@tlsldm(%reg), %eax; call ___tls_get_addr@plt, or
            // call *___tls_get_addr@GOT(%reg).
            match (byte_before(data, offset, 2), byte_after(data, offset, 4)) {
                (Some(0x8d), Some(0xe8 | 0xff)) => class(K::LdToLe, W::None).skipping(),
                _ => return Err(ClassifyError::BadTlsInstruction),
            }
        }
        R_386_TLS_GOTDESC => match context.tls {
            TlsMode::LocalExec => class(K::DescToLe, W::None),
            TlsMode::InitialExec => class(K::DescToIe, W::None),
            TlsMode::Dynamic => got(K::GotSlotRel, W::Any32, GotKind::TlsDesc),
        },
        R_386_TLS_DESC_CALL => match context.tls {
            TlsMode::Dynamic => class(K::None, W::None),
            _ => class(K::DescCallToLe, W::None),
        },
        _ => return Err(ClassifyError::Unsupported),
    })
}

fn slot<const N: usize>(out: &mut [u8], at: u64) -> Result<&mut [u8; N], ApplyError> {
    let start = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    out.get_mut(start..)
        .and_then(|rest| rest.first_chunk_mut::<N>())
        .ok_or(ApplyError::OutOfBounds)
}

fn get(out: &[u8], at: u64, back: u64) -> Result<u8, ApplyError> {
    byte_before(out, at, back).ok_or(ApplyError::BadInstruction)
}

fn put(out: &mut [u8], at: u64, back: i64, value: u8) -> Result<(), ApplyError> {
    let position = at.checked_add_signed(back).ok_or(ApplyError::OutOfBounds)?;
    *slot::<1>(out, position)? = [value];
    Ok(())
}

/// Writes a 32-bit field, wrapping as the processor does in 32-bit mode.
fn write_u32(out: &mut [u8], at: u64, value: i64) -> Result<(), ApplyError> {
    *slot::<4>(out, at)? = (value as u32).to_le_bytes();
    Ok(())
}

fn copy(out: &mut [u8], at: u64, bytes: &[u8]) -> Result<(), ApplyError> {
    let at = usize::try_from(at).map_err(|_| ApplyError::OutOfBounds)?;
    let end = at.checked_add(bytes.len()).ok_or(ApplyError::OutOfBounds)?;
    out.get_mut(at..end)
        .ok_or(ApplyError::BadInstruction)?
        .copy_from_slice(bytes);
    Ok(())
}

/// Rewrites a relaxed `R_386_GOT32X` instruction and writes `value`:
/// `S + A - GOT` for [`Kind::RelaxGotOff`], `S + A` for
/// [`Kind::RelaxGotPcNoPic`] and `S + A - P` for [`Kind::RelaxGotPc`].
///
/// # Errors
///
/// [`ApplyError`] if the instruction is not one that was classified as
/// relaxable.
pub fn relax_got(out: &mut [u8], offset: u64, kind: Kind, value: i64) -> Result<(), ApplyError> {
    let op = get(out, offset, 2)?;
    let modrm = get(out, offset, 1)?;
    match (kind, op) {
        (Kind::RelaxGotOff, 0x8b) => {
            // mov foo@GOT(%reg), %reg2 -> lea foo@GOTOFF(%reg), %reg2
            put(out, offset, -2, 0x8d)?;
            write_u32(out, offset, value)
        }
        (Kind::RelaxGotPcNoPic, 0x8b) => {
            // mov foo@GOT, %reg -> mov $foo, %reg
            put(out, offset, -2, 0xc7)?;
            put(out, offset, -1, 0xc0 | ((modrm >> 3) & 7))?;
            write_u32(out, offset, value)
        }
        (Kind::RelaxGotPc, 0xff) if modrm & 0x38 == 0x10 => {
            // call *foo@GOT(%reg) -> addr32 call foo; the displacement is
            // relative to the end of the instruction.
            put(out, offset, -2, 0x67)?;
            put(out, offset, -1, 0xe8)?;
            write_u32(out, offset, value.wrapping_sub(4))
        }
        (Kind::RelaxGotPc, 0xff) if modrm & 0x38 == 0x20 => {
            // jmp *foo@GOT(%reg) -> jmp foo; nop
            put(out, offset, -2, 0xe9)?;
            put(out, offset, 3, 0x90)?;
            let at = offset.checked_sub(1).ok_or(ApplyError::OutOfBounds)?;
            write_u32(out, at, value.wrapping_sub(3))
        }
        _ => Err(ApplyError::BadInstruction),
    }
}

/// Where a general-dynamic `lea` starts, given its displacement at
/// `offset`: three bytes back for the SIB form, two otherwise.
fn gd_start(out: &[u8], offset: u64) -> Result<(u64, bool), ApplyError> {
    let sib = get(out, offset, 2)? == 0x04;
    let back = if sib { 3 } else { 2 };
    Ok((
        offset.checked_sub(back).ok_or(ApplyError::BadInstruction)?,
        sib,
    ))
}

/// Relaxes a TLS access to local-exec or initial-exec.
///
/// # Errors
///
/// [`ApplyError`] for unrecognized instruction sequences.
pub fn relax_tls(
    out: &mut [u8],
    offset: u64,
    kind: Kind,
    r_type: u32,
    values: RelaxValues,
) -> Result<(), ApplyError> {
    let tpoff = values.tpoff;
    match kind {
        Kind::GdToLe => {
            // leal x@tlsgd(,%ebx,1),%eax; call ___tls_get_addr@plt (or the
            // %reg form with an indirect call) ->
            // movl %gs:0,%eax; subl $-x@ntpoff,%eax
            let (start, _) = gd_start(out, offset)?;
            copy(
                out,
                start,
                &[0x65, 0xa1, 0, 0, 0, 0, 0x81, 0xe8, 0, 0, 0, 0],
            )?;
            write_u32(
                out,
                start.checked_add(8).ok_or(ApplyError::OutOfBounds)?,
                tpoff.wrapping_neg(),
            )
        }
        Kind::GdToIe => {
            // -> movl %gs:0,%eax; addl x@gotntpoff(%reg),%eax
            let (start, sib) = gd_start(out, offset)?;
            let base = if sib {
                0x83
            } else {
                0x80 | (get(out, offset, 1)? & 7)
            };
            copy(
                out,
                start,
                &[0x65, 0xa1, 0, 0, 0, 0, 0x03, base, 0, 0, 0, 0],
            )?;
            let got_off = values.got.wrapping_sub(values.got_base) as i64;
            write_u32(
                out,
                start.checked_add(8).ok_or(ApplyError::OutOfBounds)?,
                got_off,
            )
        }
        Kind::LdToLe => {
            let start = offset.checked_sub(2).ok_or(ApplyError::BadInstruction)?;
            if byte_after(out, offset, 4) == Some(0xe8) {
                // leal x@tlsldm(%reg),%eax; call ___tls_get_addr@plt ->
                // movl %gs:0,%eax; nop; leal 0(%esi,1),%esi
                copy(
                    out,
                    start,
                    &[0x65, 0xa1, 0, 0, 0, 0, 0x90, 0x8d, 0x74, 0x26, 0x00],
                )
            } else {
                // ...; call *___tls_get_addr@GOT(%reg) ->
                // movl %gs:0,%eax; leal 0(%esi),%esi
                copy(
                    out,
                    start,
                    &[0x65, 0xa1, 0, 0, 0, 0, 0x8d, 0xb6, 0, 0, 0, 0],
                )
            }
        }
        Kind::IeToLe => {
            let op = get(out, offset, 2)?;
            let modrm = get(out, offset, 1)?;
            let reg = (modrm >> 3) & 7;
            if r_type == R_386_TLS_IE {
                if modrm == 0xa1 {
                    // movl foo@indntpoff,%eax -> movl $foo,%eax
                    put(out, offset, -1, 0xb8)?;
                } else if op == 0x8b {
                    // movl foo@indntpoff,%reg -> movl $foo,%reg
                    put(out, offset, -2, 0xc7)?;
                    put(out, offset, -1, 0xc0 | reg)?;
                } else if op == 0x03 {
                    // addl foo@indntpoff,%reg -> addl $foo,%reg
                    put(out, offset, -2, 0x81)?;
                    put(out, offset, -1, 0xc0 | reg)?;
                } else {
                    return Err(ApplyError::BadInstruction);
                }
            } else if op == 0x8b {
                // movl foo@gotntpoff(%reg),%reg2 -> movl $foo,%reg2
                put(out, offset, -2, 0xc7)?;
                put(out, offset, -1, 0xc0 | reg)?;
            } else if op == 0x03 {
                // addl foo@gotntpoff(%reg),%reg2 -> leal foo(%reg2),%reg2
                put(out, offset, -2, 0x8d)?;
                put(out, offset, -1, 0x80 | (reg << 3) | reg)?;
            } else {
                return Err(ApplyError::BadInstruction);
            }
            write_u32(out, offset, tpoff)
        }
        Kind::DescToLe => {
            // leal x@tlsdesc(%ebx),%eax -> leal x@ntpoff,%eax
            if get(out, offset, 2)? != 0x8d || get(out, offset, 1)? != 0x83 {
                return Err(ApplyError::BadInstruction);
            }
            put(out, offset, -1, 0x05)?;
            write_u32(out, offset, tpoff)
        }
        Kind::DescToIe => {
            // leal x@tlsdesc(%ebx),%eax -> movl x@gotntpoff(%ebx),%eax
            if get(out, offset, 2)? != 0x8d {
                return Err(ApplyError::BadInstruction);
            }
            put(out, offset, -2, 0x8b)?;
            write_u32(out, offset, values.got.wrapping_sub(values.got_base) as i64)
        }
        Kind::DescCallToLe => {
            // call *x@tlscall(%eax) -> xchg %ax,%ax
            copy(out, offset, &[0x66, 0x90])
        }
        _ => Err(ApplyError::BadInstruction),
    }
}

/// Size of a PLT entry (every kind with IBT, and `.plt` entries without).
pub const PLT_ENTRY_SIZE: u64 = 16;
/// Size of a `.plt.got` entry without IBT.
pub const PLT_GOT_ENTRY_SIZE: u64 = 8;

fn put_bytes(out: &mut [u8], at: usize, bytes: &[u8]) -> Result<(), ApplyError> {
    let end = at.checked_add(bytes.len()).ok_or(ApplyError::OutOfBounds)?;
    out.get_mut(at..end)
        .ok_or(ApplyError::OutOfBounds)?
        .copy_from_slice(bytes);
    Ok(())
}

fn rel32(target: u64, next_instruction: u64) -> [u8; 4] {
    (target.wrapping_sub(next_instruction) as u32).to_le_bytes()
}

/// The `jmp *` that goes through the GOT word at `slot`: absolute in
/// position-dependent output, `%ebx`-relative (to `got`, the GOT base)
/// otherwise.
fn jump_through(slot: u64, got: u64, pic: bool) -> [u8; 6] {
    let (modrm, operand) = if pic {
        (0xa3, slot.wrapping_sub(got) as u32)
    } else {
        (0x25, slot as u32)
    };
    let [a, b, c, d] = operand.to_le_bytes();
    [0xff, modrm, a, b, c, d]
}

/// Writes the lazy PLT header at `plt`: push the link map word and jump to
/// the resolver, both in `.got.plt` (at `got_plt`, the GOT base).
///
/// # Errors
///
/// [`ApplyError`] when out of range.
pub fn write_plt_header(out: &mut [u8], got_plt: u64, pic: bool) -> Result<(), ApplyError> {
    if pic {
        // pushl 4(%ebx); jmp *8(%ebx)
        put_bytes(out, 0, &[0xff, 0xb3, 4, 0, 0, 0, 0xff, 0xa3, 8, 0, 0, 0])?;
    } else {
        put_bytes(out, 0, &[0xff, 0x35])?;
        put_bytes(out, 2, &(got_plt.wrapping_add(4) as u32).to_le_bytes())?;
        put_bytes(out, 6, &[0xff, 0x25])?;
        put_bytes(out, 8, &(got_plt.wrapping_add(8) as u32).to_le_bytes())?;
    }
    put_bytes(out, 12, &[0x0f, 0x1f, 0x40, 0x00])
}

/// Writes lazy `.plt` entry `index` at `entry`. Without IBT it jumps
/// through its `.got.plt` slot at `slot` (which initially points back at
/// the `push`); with IBT it is only the lazy-binding half, and the jump is
/// in `.plt.sec`.
///
/// # Errors
///
/// [`ApplyError`] when out of range.
#[allow(clippy::too_many_arguments)]
pub fn write_plt_entry(
    out: &mut [u8],
    entry: u64,
    slot: u64,
    index: u32,
    plt: u64,
    got: u64,
    pic: bool,
    ibt: bool,
) -> Result<(), ApplyError> {
    // The push operand is the byte offset of the entry's `R_386_JUMP_SLOT`
    // in `.rel.plt`.
    let offset = index.wrapping_mul(8);
    if ibt {
        put_bytes(out, 0, &[0xf3, 0x0f, 0x1e, 0xfb, 0x68])?;
        put_bytes(out, 5, &offset.to_le_bytes())?;
        put_bytes(out, 9, &[0xe9])?;
        put_bytes(out, 10, &rel32(plt, entry.wrapping_add(14)))?;
        put_bytes(out, 14, &[0x66, 0x90])
    } else {
        put_bytes(out, 0, &jump_through(slot, got, pic))?;
        put_bytes(out, 6, &[0x68])?;
        put_bytes(out, 7, &offset.to_le_bytes())?;
        put_bytes(out, 11, &[0xe9])?;
        put_bytes(out, 12, &rel32(plt, entry.wrapping_add(16)))
    }
}

/// Writes a `.plt.sec` entry (IBT) or `.plt.got` entry that jumps through
/// the GOT word at `slot`. `ibt` selects the 16-byte form with `endbr32`;
/// otherwise the 8-byte `.plt.got` form is written.
///
/// # Errors
///
/// [`ApplyError`] when out of range.
pub fn write_plt_jump(
    out: &mut [u8],
    slot: u64,
    got: u64,
    pic: bool,
    ibt: bool,
) -> Result<(), ApplyError> {
    if ibt {
        put_bytes(out, 0, &[0xf3, 0x0f, 0x1e, 0xfb])?;
        put_bytes(out, 4, &jump_through(slot, got, pic))?;
        put_bytes(out, 10, &[0x66, 0x0f, 0x1f, 0x44, 0x00, 0x00])
    } else {
        put_bytes(out, 0, &jump_through(slot, got, pic))?;
        put_bytes(out, 6, &[0x66, 0x90])
    }
}

/// Size of an IFUNC PLT stub.
pub const IPLT_ENTRY_SIZE: u64 = 16;

/// Writes an IFUNC stub that jumps through the GOT slot at `slot`, padded
/// with `nop`s.
///
/// # Errors
///
/// [`ApplyError::OutOfBounds`] when `out` is too small.
pub fn write_iplt(out: &mut [u8], slot: u64, got: u64, pic: bool) -> Result<(), ApplyError> {
    put_bytes(out, 0, &jump_through(slot, got, pic))?;
    put_bytes(
        out,
        6,
        &[0x66, 0x0f, 0x1f, 0x44, 0x00, 0x00, 0x66, 0x0f, 0x1f, 0x44],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(tls: TlsMode) -> ClassifyContext {
        ClassifyContext {
            tls,
            tls_ld: tls,
            code: true,
            ..ClassifyContext::static_exec(true)
        }
    }

    #[test]
    fn reads_implicit_addends() {
        let data = [0xfc, 0xff, 0xff, 0xff, 0x10, 0x00, 0x80];
        assert_eq!(implicit_addend(R_386_PC32, &data, 0), -4);
        assert_eq!(implicit_addend(R_386_16, &data, 4), 0x10);
        assert_eq!(implicit_addend(R_386_8, &data, 6), -128);
        assert_eq!(implicit_addend(R_386_NONE, &data, 0), 0);
        assert_eq!(implicit_addend(R_386_32, &data, 5), 0);
    }

    #[test]
    fn relaxes_general_dynamic_to_local_exec() {
        // leal x@tlsgd(,%ebx,1),%eax; call ___tls_get_addr@plt
        let mut code = vec![0x8d, 0x04, 0x1d, 0, 0, 0, 0, 0xe8, 0, 0, 0, 0];
        let class = classify(R_386_TLS_GD, &code, 3, context(TlsMode::LocalExec)).unwrap();
        assert_eq!(class.kind, Kind::GdToLe);
        assert!(class.skip_next);
        let values = RelaxValues {
            tpoff: -8,
            ..RelaxValues::default()
        };
        relax_tls(&mut code, 3, Kind::GdToLe, R_386_TLS_GD, values).unwrap();
        assert_eq!(
            code,
            [0x65, 0xa1, 0, 0, 0, 0, 0x81, 0xe8, 8, 0, 0, 0],
            "movl %gs:0,%eax; subl $8,%eax"
        );
    }

    #[test]
    fn relaxes_general_dynamic_to_initial_exec() {
        // leal x@tlsgd(%ebx),%eax; call *___tls_get_addr@GOT(%ebx)
        let mut code = vec![0x8d, 0x83, 0, 0, 0, 0, 0xff, 0x93, 0, 0, 0, 0];
        let class = classify(R_386_TLS_GD, &code, 2, context(TlsMode::InitialExec)).unwrap();
        assert_eq!(class.kind, Kind::GdToIe);
        let values = RelaxValues {
            got: 0x2010,
            got_base: 0x2000,
            ..RelaxValues::default()
        };
        relax_tls(&mut code, 2, Kind::GdToIe, R_386_TLS_GD, values).unwrap();
        assert_eq!(code, [0x65, 0xa1, 0, 0, 0, 0, 0x03, 0x83, 0x10, 0, 0, 0]);
    }

    #[test]
    fn relaxes_local_dynamic_to_local_exec() {
        // leal x@tlsldm(%ebx),%eax; call ___tls_get_addr@plt
        let mut code = vec![0x8d, 0x83, 0, 0, 0, 0, 0xe8, 0, 0, 0, 0];
        let class = classify(R_386_TLS_LDM, &code, 2, context(TlsMode::LocalExec)).unwrap();
        assert_eq!(class.kind, Kind::LdToLe);
        relax_tls(
            &mut code,
            2,
            Kind::LdToLe,
            R_386_TLS_LDM,
            RelaxValues::default(),
        )
        .unwrap();
        assert_eq!(code, [0x65, 0xa1, 0, 0, 0, 0, 0x90, 0x8d, 0x74, 0x26, 0x00]);
    }

    #[test]
    fn relaxes_initial_exec_to_local_exec() {
        // movl x@gotntpoff(%ebx),%ecx
        let mut code = vec![0x8b, 0x8b, 0, 0, 0, 0];
        let values = RelaxValues {
            tpoff: -4,
            ..RelaxValues::default()
        };
        relax_tls(&mut code, 2, Kind::IeToLe, R_386_TLS_GOTIE, values).unwrap();
        assert_eq!(code, [0xc7, 0xc1, 0xfc, 0xff, 0xff, 0xff]);
        // addl x@gotntpoff(%ebx),%edx -> leal x(%edx),%edx
        let mut code = vec![0x03, 0x93, 0, 0, 0, 0];
        relax_tls(&mut code, 2, Kind::IeToLe, R_386_TLS_GOTIE, values).unwrap();
        assert_eq!(code, [0x8d, 0x92, 0xfc, 0xff, 0xff, 0xff]);
        // movl x@indntpoff,%eax (a1) -> movl $x,%eax (b8)
        let mut code = vec![0x90, 0xa1, 0, 0, 0, 0];
        relax_tls(&mut code, 2, Kind::IeToLe, R_386_TLS_IE, values).unwrap();
        assert_eq!(code, [0x90, 0xb8, 0xfc, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn relaxes_got32x() {
        // movl foo@GOT(%ebx),%eax -> leal foo@GOTOFF(%ebx),%eax
        let mut code = vec![0x8b, 0x83, 0, 0, 0, 0];
        let class = classify(R_386_GOT32X, &code, 2, context(TlsMode::LocalExec)).unwrap();
        assert_eq!(class.kind, Kind::RelaxGotOff);
        relax_got(&mut code, 2, Kind::RelaxGotOff, 0x40).unwrap();
        assert_eq!(code, [0x8d, 0x83, 0x40, 0, 0, 0]);
        // call *foo@GOT(%ebx) -> addr32 call foo
        let mut code = vec![0xff, 0x93, 0, 0, 0, 0];
        let class = classify(R_386_GOT32X, &code, 2, context(TlsMode::LocalExec)).unwrap();
        assert_eq!(class.kind, Kind::RelaxGotPc);
        relax_got(&mut code, 2, Kind::RelaxGotPc, 0x104).unwrap();
        assert_eq!(code, [0x67, 0xe8, 0x00, 0x01, 0, 0]);
        // A preemptible symbol keeps its GOT load.
        let code = vec![0x8b, 0x83, 0, 0, 0, 0];
        let mut dynamic = context(TlsMode::Dynamic);
        dynamic.relax_got = false;
        let class = classify(R_386_GOT32X, &code, 2, dynamic).unwrap();
        assert_eq!(class.kind, Kind::GotSlotRel);
    }

    #[test]
    fn writes_plt_entries() {
        let mut entry = [0u8; 16];
        write_plt_entry(&mut entry, 0x1010, 0x3010, 2, 0x1000, 0x3000, false, false).unwrap();
        assert_eq!(
            entry,
            [
                0xff, 0x25, 0x10, 0x30, 0, 0, 0x68, 16, 0, 0, 0, 0xe9, 0xe0, 0xff, 0xff, 0xff
            ]
        );
        write_plt_entry(&mut entry, 0x1010, 0x3010, 2, 0x1000, 0x3000, true, false).unwrap();
        assert_eq!(&entry[..6], &[0xff, 0xa3, 0x10, 0, 0, 0]);
    }
}
