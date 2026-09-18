//! Writing and relocating RISC-V input sections.
//!
//! The writer's generic loop applies one relocation at a time; RISC-V needs
//! more context, so its input sections come here instead:
//!
//! - the section is copied with the relaxation edits of layout applied
//!   ([`super::relax`]): deleted bytes dropped, `R_RISCV_ALIGN` padding
//!   rewritten, relaxed calls and local-exec accesses replaced;
//! - relocation offsets move with the edits;
//! - a `%pcrel_lo` takes the low part of the value its `%pcrel_hi` (found by
//!   the label it names) computes, at the `auipc`'s address;
//! - TLS descriptor sequences are rewritten as a whole, as lld does;
//! - `SET_ULEB128`/`SUB_ULEB128` pairs write a label difference;
//! - `.riscv.attributes` sections are replaced by the merged one.
//!
//! Everything else — GOT and PLT redirection, dynamic relocations, TLS
//! offsets, tombstones for dead code in debug sections — follows the same
//! [`reloc::decide`] decisions as the scan and the other architectures.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::riscv::{
    self as insn, A0, ADDI, AUIPC, FieldError, LD, LUI, NOP, fits_signed, hi20, itype, lo12, utype,
};
use crate::debug::tombstone::DeadTarget;
use crate::diag::Diagnostic;
use crate::elf::export::PREEMPTIBLE;
use crate::elf::object::InputSection;
use crate::elf::read::consts::riscv::*;
use crate::elf::read::consts::{SHF_ALLOC, SHT_RISCV_ATTRIBUTES};
use crate::elf::read::{Relocation, Relocations};
use crate::elf::refs::{Def, Target};
use crate::elf::reloc::{self, Dynamic};
use crate::elf::scan::location;
use crate::elf::values::Addresses;
use crate::elf::write::{self, WriteInput};
use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::symbols::SymbolFlags;

use super::super::shrink::{self, Rewrite, SectionRelax};
use super::super::{ApplyError, Class, GotKind, Kind, Width};
use super::relax::{GLOBAL_POINTER, GPREL, X0REL, fill_nops};

/// One input section being written.
pub struct SectionWrite<'s> {
    /// The section.
    pub id: SectionId,
    /// Its file.
    pub file: usize,
    /// Its index in the file.
    pub index: u32,
    /// Its header and kind.
    pub section: &'s InputSection<'s>,
    /// Its input contents.
    pub data: &'s [u8],
    /// Its output address.
    pub base: u64,
}

/// What a relocation writes.
enum Value {
    /// This value, packed into the relocation's field.
    Write(u64),
    /// Nothing (a dynamic relocation supplies it, or it was reported).
    Skip,
}

struct Writer<'s, 'w, 'x, 'a> {
    input: &'s WriteInput<'w, 'x, 'a>,
    section: SectionWrite<'s>,
    alloc: bool,
    relax: Option<&'s SectionRelax>,
    tombstone: crate::debug::tombstone::SectionTombstone,
}

/// Copies and relocates one RISC-V input section into `out`.
///
/// # Errors
///
/// Relocation section parse errors; relocation problems are reported to the
/// diagnostic sink instead.
pub fn write_section(
    input: &WriteInput<'_, '_, '_>,
    section: SectionWrite<'_>,
    out: &mut [u8],
) -> Result<()> {
    let addresses = input.addresses;
    let refs = &addresses.refs;
    if section.section.header.sh_type == SHT_RISCV_ATTRIBUTES
        && let Some(merged) = &addresses.synth.riscv_attributes
    {
        if merged.first == section.id {
            if let Some(dest) = out.get_mut(..merged.bytes.len()) {
                dest.copy_from_slice(&merged.bytes);
            }
            for problem in &merged.problems {
                input.diagnostics.emit(Diagnostic::warning(problem.clone()));
            }
        }
        return Ok(());
    }
    let object = refs
        .files
        .get(section.file)
        .and_then(|f| f.object.as_ref())
        .ok_or_else(|| Error::Internal("unparsed file in output".into()))?;
    let relocations = if section.section.relocs == 0 {
        None
    } else {
        object
            .section(section.section.relocs)
            .map(|r| {
                object
                    .elf
                    .relocation_section(section.section.relocs, &r.header)
            })
            .transpose()?
            .flatten()
    };
    let relas = match relocations.map(|r| r.relocations) {
        Some(Relocations::Rela(relas)) => relas.iter().collect::<Vec<Relocation>>(),
        _ => Vec::new(),
    };
    let (ordered, _) = shrink::ordered(relas);
    let relax = addresses.layout.relax.section(section.id);
    match relax {
        Some(relax) => shrink::copy(section.data, relax, out, fill_nops),
        None => {
            if let Some(dest) = out.get_mut(..section.data.len()) {
                dest.copy_from_slice(section.data);
            }
        }
    }
    if ordered.is_empty() {
        return Ok(());
    }
    let alloc = section.section.header.sh_flags & SHF_ALLOC != 0;
    let tombstone = if alloc {
        crate::debug::tombstone::SectionTombstone::default()
    } else {
        input.tombstones.for_section(section.section.name)
    };
    let writer = Writer {
        input,
        section,
        alloc,
        relax,
        tombstone,
    };
    writer.relocate(&ordered, out);
    Ok(())
}

impl<'w, 'x, 'a> Writer<'_, 'w, 'x, 'a> {
    fn addresses(&self) -> &'w Addresses<'x, 'a> {
        self.input.addresses
    }

    fn report(&self, rel: &Relocation, message: String) {
        let refs = &self.addresses().refs;
        let order = refs
            .files
            .get(self.section.file)
            .map_or(0, |f| f.position.raw());
        self.input.diagnostics.emit(
            Diagnostic::error(message)
                .at(location(
                    refs,
                    self.section.file,
                    self.section.index,
                    rel.offset,
                ))
                .order(order),
        );
    }

    fn report_apply(&self, rel: &Relocation, error: ApplyError) {
        let arch = self.input.context.arch;
        let type_name = arch.reloc_label(rel.r_type);
        let name = write::symbol_name(&self.addresses().refs, self.section.file, rel.symbol);
        let message = match error {
            ApplyError::Overflow => {
                format!("relocation {type_name} out of range; references '{name}'")
            }
            ApplyError::OutOfBounds => {
                format!("relocation {type_name} is outside its section; references '{name}'")
            }
            ApplyError::BadInstruction => format!(
                "relocation {type_name} cannot be applied to this instruction; references '{name}'"
            ),
        };
        self.report(rel, message);
    }

    /// Where offset `offset` of the input section is in the output
    /// section's bytes.
    fn out_offset(&self, offset: u64) -> u64 {
        self.relax.map_or(offset, |r| r.map(offset))
    }

    fn put(&self, out: &mut [u8], rel: &Relocation, at: u64, width: Width, value: u64) {
        if let Err(error) = super::super::write_value(out, at, width, value) {
            self.report_apply(rel, error);
        }
    }

    /// Writes a relocation's value: into its field, or added to (`ADD*`)
    /// or subtracted from (`SUB*`) the field's contents.
    fn store(&self, out: &mut [u8], rel: &Relocation, at: u64, class: Class, value: u64) {
        let result = match class.kind {
            Kind::Add => super::super::add_value(out, at, class.width, value),
            Kind::Sub => super::super::add_value(out, at, class.width, value.wrapping_neg()),
            _ => super::super::write_value(out, at, class.width, value),
        };
        if let Err(error) = result {
            self.report_apply(rel, error);
        }
    }

    fn put_word(&self, out: &mut [u8], rel: &Relocation, at: u64, word: u32) {
        let written = usize::try_from(at)
            .ok()
            .and_then(|at| insn::write32(out, at, word));
        if written.is_none() {
            self.report_apply(rel, ApplyError::OutOfBounds);
        }
    }

    fn target(&self, rel: &Relocation) -> Option<(Target, SymbolFlags)> {
        let refs = &self.addresses().refs;
        let target = refs.target(self.section.file, rel.symbol as usize)?;
        let flags = target
            .global
            .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
        Some((target, flags))
    }

    fn decide(
        &self,
        rel: &Relocation,
        target: &Target,
        flags: SymbolFlags,
    ) -> Option<reloc::Decision> {
        reloc::decide(
            &self.input.context,
            rel,
            self.section.data,
            target,
            flags,
            self.section.section.header.sh_flags,
        )
        .ok()
    }

    /// The value relocation `rel` at `place` computes, as the generic
    /// writer computes it for other architectures. `report` is false when
    /// the value is looked up for a `%pcrel_lo`, whose `%pcrel_hi` reports
    /// its own problems.
    #[allow(clippy::too_many_lines)]
    fn value(&self, rel: &Relocation, place: u64, report: bool) -> (Class, Value) {
        let none = Class::new(Kind::None, Width::None);
        let addresses = self.addresses();
        let refs = &addresses.refs;
        let Some((target, flags)) = self.target(rel) else {
            return (none, Value::Skip);
        };
        if report
            && let Some((from, to)) =
                write::prohibited_cross_reference(self.input, self.section.id, &target)
        {
            let name = write::cross_reference_name(refs, self.section.file, rel.symbol, &target);
            self.report(
                rel,
                format!("prohibited cross reference from {from} to `{name}' in {to}"),
            );
        }
        let Some(decision) = self.decide(rel, &target, flags) else {
            return (none, Value::Skip); // Reported by the scan.
        };
        let class = decision.class;
        if class.kind == Kind::None || (self.alloc && decision.problem.is_some()) {
            return (class, Value::Skip);
        }
        let label_math = matches!(class.kind, Kind::Add | Kind::Sub)
            || matches!(class.width, Width::RiscV(field) if field.is_label_math());
        let truncated = |value: u64| {
            crate::debug::tombstone::truncate(value, super::super::width_bytes(class.width))
        };
        if !self.alloc
            && !label_math
            && matches!(class.kind, Kind::Abs | Kind::DtpOff)
            && let Some(dead) = write::dead_target(refs, &target)
            && let Some(value) = self.tombstone.get(dead)
        {
            return (class, Value::Write(truncated(value)));
        }
        let owner = Addresses::owner(&target, self.section.file, rel.symbol);
        let Some((mut s, a)) = addresses.symbol_address(&target, rel.addend) else {
            if self.alloc {
                if report {
                    let name = write::symbol_name(refs, self.section.file, rel.symbol);
                    self.report(
                        rel,
                        format!("relocation refers to a symbol in a discarded section: {name}"),
                    );
                }
                return (class, Value::Skip);
            }
            if label_math {
                return (class, Value::Write(0));
            }
            let value = self.tombstone.get(DeadTarget::Discarded).unwrap_or(0);
            return (class, Value::Write(truncated(value)));
        };
        let mut via_plt = false;
        if self.alloc {
            if target.is_ifunc()
                && let Some(stub) = addresses.iplt_address(owner)
            {
                s = stub;
                via_plt = true;
            }
            if class.kind == Kind::Pc
                && super::is_branch(rel.r_type)
                && flags.contains(SymbolFlags::NEEDS_PLT | PREEMPTIBLE)
                && let Some(plt) = addresses.plt_address(owner)
            {
                s = plt;
                via_plt = true;
            }
        }
        let undefined = matches!(target.def, Def::Undefined { .. }) && !via_plt;
        let sa = s.wrapping_add_signed(a);
        let tls = addresses.layout.tls.unwrap_or_default();
        let tp = tls.tp(self.input.context.arch);
        let got = |kind: GotKind| addresses.got_entry_address(owner, kind);
        let value = match class.kind {
            Kind::Add | Kind::Sub => sa,
            Kind::Abs => match decision.dynamic {
                Dynamic::Symbolic(_) => return (class, Value::Skip),
                _ => sa,
            },
            // A branch to an undefined weak symbol branches to itself (lld);
            // other PC-relative references compute `0 + A - P`.
            Kind::Pc if undefined && is_branch_like(rel.r_type) => a as u64,
            Kind::Pc => sa.wrapping_sub(place),
            Kind::Got => match got(class.slot) {
                Some(g) => g.wrapping_add_signed(a).wrapping_sub(place),
                None => {
                    if report {
                        self.report_apply(rel, ApplyError::BadInstruction);
                    }
                    return (class, Value::Skip);
                }
            },
            Kind::TpOff | Kind::DtpOff if undefined => a as u64,
            Kind::TpOff => sa.wrapping_sub(tp),
            Kind::DtpOff => {
                let executable =
                    self.input.context.mode.executable() || !self.input.context.mode.dynamic;
                if self.alloc && executable {
                    sa.wrapping_sub(tp)
                } else {
                    sa.wrapping_sub(tls.start)
                }
            }
            _ => {
                if report {
                    self.report_apply(rel, ApplyError::BadInstruction);
                }
                return (class, Value::Skip);
            }
        };
        (class, Value::Write(value))
    }

    /// The `%pcrel_hi` value a `%pcrel_lo` at `rel` refers to.
    fn pcrel_hi_value(&self, rel: &Relocation, relocs: &[Relocation]) -> Option<u64> {
        let (target, _) = self.target(rel)?;
        let Def::Section {
            file,
            section,
            value,
        } = target.def
        else {
            self.report(
                rel,
                format!(
                    "{} relocation points to an absolute symbol",
                    self.input.context.arch.reloc_label(rel.r_type)
                ),
            );
            return None;
        };
        let label = value.wrapping_add_signed(rel.addend);
        let start = relocs.partition_point(|r| r.offset < label);
        let hi = (file == self.section.file && section == self.section.index)
            .then(|| {
                relocs
                    .get(start..)
                    .unwrap_or_default()
                    .iter()
                    .take_while(|r| r.offset == label)
                    .find(|r| super::is_pcrel_hi(r.r_type))
            })
            .flatten();
        let Some(hi) = hi else {
            let name = write::symbol_name(&self.addresses().refs, self.section.file, rel.symbol);
            self.report(
                rel,
                format!("unable to find corresponding R_RISCV_PCREL_HI20 relocation; references '{name}'"),
            );
            return None;
        };
        let place = self.section.base.wrapping_add(self.out_offset(hi.offset));
        match self.value(hi, place, false) {
            (_, Value::Write(value)) => Some(value),
            (_, Value::Skip) => None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn relocate(&self, relocs: &[Relocation], out: &mut [u8]) {
        let addresses = self.addresses();
        let base = self.section.base;
        // The TLS descriptor sequence being written: the value its tail
        // uses, whether it is relaxed to local-exec (or initial-exec), and
        // whether its instructions were deleted.
        let mut desc_value = 0u64;
        let mut desc_exec = false;
        let mut desc_to_le = false;
        let mut desc_relaxed = false;
        let mut skip_next = false;
        for (seq, rel) in (0u32..).zip(relocs) {
            if skip_next {
                skip_next = false;
                continue;
            }
            let edit = self.relax.and_then(|r| r.edit(seq));
            let at = self.out_offset(rel.offset);
            let place = base.wrapping_add(at);
            match rel.r_type {
                R_RISCV_NONE | R_RISCV_RELAX | R_RISCV_ALIGN | R_RISCV_TPREL_ADD
                | R_RISCV_VENDOR => continue,
                R_RISCV_PCREL_LO12_I | R_RISCV_PCREL_LO12_S => {
                    if let Some(value) = self.pcrel_hi_value(rel, relocs) {
                        let field = if rel.r_type == R_RISCV_PCREL_LO12_I {
                            insn::Field::Lo12I
                        } else {
                            insn::Field::Lo12S
                        };
                        self.put(out, rel, at, Width::RiscV(field), value);
                    }
                    continue;
                }
                R_RISCV_TLSDESC_HI20 => {
                    let Some((target, flags)) = self.target(rel) else {
                        continue;
                    };
                    let Some(decision) = self.decide(rel, &target, flags) else {
                        continue;
                    };
                    let owner = Addresses::owner(&target, self.section.file, rel.symbol);
                    desc_relaxed = edit.is_some();
                    match decision.class.kind {
                        Kind::DescToLe => {
                            desc_exec = true;
                            desc_to_le = true;
                            let tls = addresses.layout.tls.unwrap_or_default();
                            desc_value = addresses
                                .symbol_address(&target, rel.addend)
                                .map_or(0, |(s, a)| s.wrapping_add_signed(a))
                                .wrapping_sub(tls.tp(self.input.context.arch));
                        }
                        Kind::DescToIe => {
                            desc_exec = true;
                            desc_to_le = false;
                            let Some(got) = addresses.got_entry_address(owner, GotKind::TpOff)
                            else {
                                self.report_apply(rel, ApplyError::BadInstruction);
                                continue;
                            };
                            desc_value = got
                                .wrapping_add_signed(rel.addend)
                                .wrapping_sub(place)
                                .wrapping_add(at);
                        }
                        _ => {
                            desc_exec = false;
                            if let (class, Value::Write(value)) = self.value(rel, place, true) {
                                desc_value = value;
                                self.put(out, rel, at, class.width, value);
                            }
                            continue;
                        }
                    }
                    if !desc_relaxed {
                        self.put_word(out, rel, at, NOP);
                    }
                    continue;
                }
                R_RISCV_TLSDESC_LOAD_LO12 | R_RISCV_TLSDESC_ADD_LO12 | R_RISCV_TLSDESC_CALL => {
                    if !desc_exec {
                        if rel.r_type != R_RISCV_TLSDESC_CALL {
                            self.put(out, rel, at, Width::RiscV(insn::Field::Lo12I), desc_value);
                        }
                        continue;
                    }
                    if !desc_to_le && rel.r_type == R_RISCV_TLSDESC_ADD_LO12 {
                        desc_value = desc_value.wrapping_sub(at);
                    }
                    let value = desc_value;
                    let short = desc_to_le && hi20(value) == 0;
                    if desc_relaxed
                        && (rel.r_type == R_RISCV_TLSDESC_LOAD_LO12
                            || (rel.r_type == R_RISCV_TLSDESC_ADD_LO12 && short))
                    {
                        continue;
                    }
                    let word = match (rel.r_type, desc_to_le) {
                        (R_RISCV_TLSDESC_LOAD_LO12, _) => NOP,
                        (R_RISCV_TLSDESC_ADD_LO12, true) if short => NOP,
                        (R_RISCV_TLSDESC_ADD_LO12, true) => utype(LUI, A0, hi20(value)),
                        (R_RISCV_TLSDESC_ADD_LO12, false) => utype(AUIPC, A0, hi20(value)),
                        (_, true) if fits_signed(value as i64, 12) => {
                            itype(ADDI, A0, insn::X0, value as u32)
                        }
                        (_, true) => itype(ADDI, A0, A0, lo12(value)),
                        (_, false) => itype(LD, A0, A0, lo12(value)),
                    };
                    self.put_word(out, rel, at, word);
                    continue;
                }
                R_RISCV_SET_ULEB128 => {
                    let pair = relocs
                        .get((seq as usize).saturating_add(1))
                        .filter(|n| n.r_type == R_RISCV_SUB_ULEB128 && n.offset == rel.offset);
                    let Some(sub) = pair else {
                        self.report(
                            rel,
                            "R_RISCV_SET_ULEB128 not paired with R_RISCV_SUB_ULEB128".into(),
                        );
                        continue;
                    };
                    skip_next = true;
                    let address = |r: &Relocation| {
                        self.target(r)
                            .and_then(|(t, _)| addresses.symbol_address(&t, r.addend))
                            .map_or(0, |(s, a)| s.wrapping_add_signed(a))
                    };
                    let value = address(rel).wrapping_sub(address(sub));
                    let written = usize::try_from(at)
                        .ok()
                        .and_then(|at| out.get_mut(at..))
                        .map(|data| insn::write_uleb128(data, value));
                    match written {
                        Some(Ok(())) => {}
                        Some(Err(FieldError::Overflow)) => {
                            let name =
                                write::symbol_name(&addresses.refs, self.section.file, rel.symbol);
                            self.report(
                                rel,
                                format!(
                                    "ULEB128 value {value} exceeds available space; references '{name}'"
                                ),
                            );
                        }
                        _ => self.report_apply(rel, ApplyError::OutOfBounds),
                    }
                    continue;
                }
                R_RISCV_SUB_ULEB128 => {
                    self.report(
                        rel,
                        "R_RISCV_SUB_ULEB128 not paired with R_RISCV_SET_ULEB128".into(),
                    );
                    continue;
                }
                _ => {}
            }
            match edit.map(|e| e.rewrite) {
                // Deleted, or already written whole by the copy.
                Some(
                    Rewrite::Delete | Rewrite::Align { .. } | Rewrite::Replace { r_type: 0, .. },
                ) => continue,
                Some(Rewrite::Replace { r_type, .. }) => {
                    let field = if r_type == R_RISCV_RVC_JUMP {
                        insn::Field::RvcJump
                    } else {
                        insn::Field::Jal
                    };
                    if let (_, Value::Write(value)) = self.value(rel, place, true) {
                        self.put(out, rel, at, Width::RiscV(field), value);
                    }
                    continue;
                }
                Some(Rewrite::Retype(GPREL)) => {
                    let field = if rel.r_type == R_RISCV_LO12_S {
                        insn::Field::GpRelS
                    } else {
                        insn::Field::GpRelI
                    };
                    let gp = addresses
                        .refs
                        .symbols
                        .lookup(&crate::symbols::SymbolName::new(GLOBAL_POINTER))
                        .and_then(|id| addresses.globals.get(id.index()).copied())
                        .unwrap_or(0);
                    if let (_, Value::Write(value)) = self.value(rel, place, true) {
                        self.put(out, rel, at, Width::RiscV(field), value.wrapping_sub(gp));
                    }
                    continue;
                }
                Some(Rewrite::Retype(X0REL)) => {
                    let field = if rel.r_type == R_RISCV_LO12_S {
                        insn::Field::X0RelS
                    } else {
                        insn::Field::X0RelI
                    };
                    if let (_, Value::Write(value)) = self.value(rel, place, true) {
                        self.put(out, rel, at, Width::RiscV(field), value);
                    }
                    continue;
                }
                Some(Rewrite::Retype(_)) | None => {}
            }
            if let (class, Value::Write(value)) = self.value(rel, place, true) {
                self.store(out, rel, at, class, value);
            }
        }
    }
}

/// Whether an undefined weak target of `r_type` resolves to the place
/// itself, so that the branch is harmless (lld's
/// `getRISCVUndefinedRelativeWeakVA`).
fn is_branch_like(r_type: u32) -> bool {
    matches!(
        r_type,
        R_RISCV_BRANCH
            | R_RISCV_JAL
            | R_RISCV_CALL
            | R_RISCV_CALL_PLT
            | R_RISCV_RVC_BRANCH
            | R_RISCV_RVC_JUMP
            | R_RISCV_PLT32
    )
}
