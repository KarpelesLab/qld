//! Writing and relocating Arm input sections.
//!
//! The writer's generic loop applies one relocation at a time and knows
//! nothing about instruction sets; Arm branches need more, so its input
//! sections come here instead:
//!
//! - a `bl` whose target is in the other state becomes a `blx` (and back),
//!   and a branch that cannot change state, or cannot reach its target,
//!   goes to the thunk layout planned for it ([`super::thunks`]);
//! - a branch to an undefined weak symbol with no PLT entry becomes a
//!   `nop`, as in GNU ld;
//! - `.ARM.exidx` sections are replaced by the one table ([`super::exidx`])
//!   and `.ARM.attributes` sections by the merged one
//!   ([`super::attributes`]).
//!
//! Everything else — GOT and PLT redirection, dynamic relocations, TLS
//! offsets, tombstones for dead code in debug sections — follows the same
//! [`reloc::decide`] decisions as the scan and the other architectures.

#![deny(clippy::arithmetic_side_effects)]

use crate::arch::arm::{self as insn, Field};
use crate::debug::tombstone::DeadTarget;
use crate::diag::Diagnostic;
use crate::elf::export::PREEMPTIBLE;
use crate::elf::object::InputSection;
use crate::elf::read::Relocation;
use crate::elf::read::consts::{SHF_ALLOC, STT_FUNC, STT_GNU_IFUNC};
use crate::elf::refs::{Def, Target};
use crate::elf::reloc::{self, Dynamic};
use crate::elf::scan::location;
use crate::elf::values::Addresses;
use crate::elf::write::{self, WriteInput};
use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::symbols::SymbolFlags;

use super::super::{ApplyError, Arch, GotKind, Kind, Width};
use super::{Branch, SHT_ARM_ATTRIBUTES, SHT_ARM_EXIDX};

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

/// Copies and relocates one Arm input section into `out`.
///
/// # Errors
///
/// Relocation section parse errors; relocation problems are reported to
/// the diagnostic sink instead.
pub fn write_section<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    section: SectionWrite<'_>,
    out: &mut [u8],
) -> Result<()> {
    let arm = input.addresses.synth.arm.as_deref();
    match section.section.header.sh_type {
        SHT_ARM_ATTRIBUTES => {
            if let Some(merged) = arm.and_then(|a| a.attributes.as_ref())
                && merged.first == section.id
            {
                if let Some(dest) = out.get_mut(..merged.bytes.len()) {
                    dest.copy_from_slice(&merged.bytes);
                }
                for problem in &merged.problems {
                    input.diagnostics.emit(Diagnostic::warning(problem.clone()));
                }
                for problem in &merged.errors {
                    input.diagnostics.emit(Diagnostic::error(problem.clone()));
                }
            }
            return Ok(());
        }
        SHT_ARM_EXIDX => {
            if let Some(table) = arm.and_then(|a| a.exidx.as_ref())
                && table.first == section.id
            {
                write_exidx(input, table, section.base, out);
            }
            return Ok(());
        }
        _ => {}
    }
    if let Some(dest) = out.get_mut(..section.data.len()) {
        dest.copy_from_slice(section.data);
    }
    if section.section.relocs == 0 {
        return Ok(());
    }
    let object = input
        .addresses
        .refs
        .files
        .get(section.file)
        .and_then(|f| f.object.as_ref())
        .ok_or_else(|| Error::Internal("unparsed file in output".into()))?;
    let relocations = object
        .section(section.section.relocs)
        .map(|r| {
            object
                .elf
                .relocation_section(section.section.relocs, &r.header)
        })
        .transpose()?
        .flatten();
    let Some(relocations) = relocations.map(|r| r.relocations) else {
        return Ok(());
    };
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
        tombstone,
    };
    super::super::for_each_relocation!(Arch::Arm, relocations, writer.section.data, |rel| {
        writer.relocate(&rel, out);
    });
    Ok(())
}

/// Writes the exception index into `out`, the place of the first
/// `.ARM.exidx` input section.
fn write_exidx<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    table: &super::exidx::Table,
    base: u64,
    out: &mut [u8],
) {
    let addresses = input.addresses;
    let refs = &addresses.refs;
    let section_of = |id: SectionId| {
        let (file, index) = refs.sections.locate(id)?;
        let object = refs.files.get(file)?.object.as_ref()?;
        Some((file, object.section(index)?))
    };
    let contents = |id: SectionId| -> Option<&[u8]> {
        let (_, section) = section_of(id)?;
        let (file, _) = refs.sections.locate(id)?;
        let object = refs.files.get(file)?.object.as_ref()?;
        object.elf.section_data(&section.header).ok()
    };
    let size_of = |id: SectionId| section_of(id).map_or(0, |(_, s)| s.header.sh_size);
    let address = |id: SectionId| addresses.section_address(id);
    let mut relocate = |id: SectionId, place: u64, dest: &mut [u8]| {
        relocate_exidx(input, id, place, dest);
    };
    super::exidx::write(
        table,
        base,
        out,
        &address,
        &size_of,
        &contents,
        &mut relocate,
    );
}

/// Applies the relocations of `.ARM.exidx` section `id`, whose contents
/// have been copied to `dest` at address `place`. Its relocations are
/// `R_ARM_PREL31` references to the code it describes and to `.ARM.extab`
/// entries, plus `R_ARM_NONE` markers that keep a personality routine.
fn relocate_exidx<F: crate::elf::read::ElfFormat>(
    input: &WriteInput<'_, '_, '_, F>,
    id: SectionId,
    place: u64,
    dest: &mut [u8],
) {
    let addresses = input.addresses;
    let refs = &addresses.refs;
    let Some((file, index)) = refs.sections.locate(id) else {
        return;
    };
    let Some(object) = refs.files.get(file).and_then(|f| f.object.as_ref()) else {
        return;
    };
    let Some(section) = object.section(index) else {
        return;
    };
    if section.relocs == 0 {
        return;
    }
    let relocations = object
        .section(section.relocs)
        .and_then(|r| {
            object
                .elf
                .relocation_section(section.relocs, &r.header)
                .ok()?
        })
        .map(|r| r.relocations);
    let Some(relocations) = relocations else {
        return;
    };
    let data = object.elf.section_data(&section.header).unwrap_or_default();
    super::super::for_each_relocation!(Arch::Arm, relocations, data, |rel| {
        if rel.r_type != super::R_ARM_PREL31 {
            continue;
        }
        let Some(target) = refs.target(file, rel.symbol as usize) else {
            continue;
        };
        let Some((s, a)) = addresses.symbol_address(&target, rel.addend) else {
            continue;
        };
        let at = usize::try_from(rel.offset).unwrap_or(usize::MAX);
        let value = s
            .wrapping_add_signed(a)
            .wrapping_sub(place.wrapping_add(rel.offset));
        let _ = super::super::write_value(dest, at as u64, Width::Arm(Field::Prel31), value);
    });
}

struct Writer<'s, 'w, 'x, 'a, F: crate::elf::read::ElfFormat> {
    input: &'s WriteInput<'w, 'x, 'a, F>,
    section: SectionWrite<'s>,
    alloc: bool,
    tombstone: crate::debug::tombstone::SectionTombstone,
}

impl<'w, 'x, 'a, F: crate::elf::read::ElfFormat> Writer<'_, 'w, 'x, 'a, F> {
    fn addresses(&self) -> &'w Addresses<'x, 'a, F> {
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
        let type_name = Arch::Arm.reloc_label(rel.r_type);
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

    fn put(&self, out: &mut [u8], rel: &Relocation, width: Width, value: u64) {
        if let Err(error) = super::super::write_value(out, rel.offset, width, value) {
            self.report_apply(rel, error);
        }
    }

    /// Writes one relocation.
    #[allow(clippy::too_many_lines)]
    fn relocate(&self, rel: &Relocation, out: &mut [u8]) {
        let addresses = self.addresses();
        let refs = &addresses.refs;
        let context = &self.input.context;
        let section = &self.section;
        let Some(target) = refs.target(section.file, rel.symbol as usize) else {
            return;
        };
        if let Some((from, to)) = write::prohibited_cross_reference(self.input, section.id, &target)
        {
            let name = write::cross_reference_name(refs, section.file, rel.symbol, &target);
            self.report(
                rel,
                format!("prohibited cross reference from {from} to `{name}' in {to}"),
            );
        }
        let flags = target
            .global
            .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
        let Ok(decision) = reloc::decide::<F>(
            context,
            rel,
            section.data,
            &target,
            flags,
            section.section.header.sh_flags,
        ) else {
            return; // Reported by the scan.
        };
        let class = decision.class;
        if class.kind == Kind::None || (self.alloc && decision.problem.is_some()) {
            return;
        }
        let place = section.base.wrapping_add(rel.offset);
        let owner = Addresses::<F>::owner(&target, section.file, rel.symbol);
        let truncated = |value: u64| {
            crate::debug::tombstone::truncate(value, super::super::width_bytes(class.width))
        };
        if !self.alloc
            && matches!(class.kind, Kind::Abs | Kind::DtpOff)
            && let Some(dead) = write::dead_target(refs, &target)
            && let Some(value) = self.tombstone.get(dead)
        {
            self.put(out, rel, class.width, truncated(value));
            return;
        }
        let Some((mut s, a)) = addresses.symbol_address(&target, rel.addend) else {
            if self.alloc {
                let name = write::symbol_name(refs, section.file, rel.symbol);
                self.report(
                    rel,
                    format!("relocation refers to a symbol in a discarded section: {name}"),
                );
                return;
            }
            let value = self.tombstone.get(DeadTarget::Discarded).unwrap_or(0);
            self.put(out, rel, class.width, truncated(value));
            return;
        };
        let mut via_stub = false;
        if self.alloc {
            if target.is_ifunc()
                && let Some(stub) = addresses.iplt_address(owner)
            {
                s = stub;
                via_stub = true;
            }
            if flags.contains(SymbolFlags::NEEDS_PLT | PREEMPTIBLE)
                && class.kind == Kind::Pc
                && super::is_branch(rel.r_type)
                && let Some(plt) = addresses.plt_address(owner)
            {
                s = plt;
                via_stub = true;
            }
        }
        let sa = s.wrapping_add_signed(a);
        let undefined = matches!(target.def, Def::Undefined { .. }) && !via_stub;
        if super::is_thunk_branch(rel.r_type) {
            self.branch(rel, out, &target, sa, via_stub, undefined);
            return;
        }
        let tls = addresses.layout.tls.unwrap_or_default();
        let tp = tls.tp(Arch::Arm);
        let slot = |kind: GotKind| addresses.got_entry_address(owner, kind);
        let place_for_field = match class.width {
            Width::Arm(field) if field.from_aligned_place() => place & !3,
            _ => place,
        };
        let value = match class.kind {
            Kind::Abs => match decision.dynamic {
                // `SHT_REL`: the addend stays in the field for the dynamic
                // linker to add the symbol's address to.
                Dynamic::Symbolic(_) => return,
                _ => sa,
            },
            Kind::Pc => sa.wrapping_sub(place_for_field),
            Kind::Got => match slot(class.slot) {
                Some(g) => g.wrapping_add_signed(a).wrapping_sub(place),
                None => {
                    self.report_apply(rel, ApplyError::BadInstruction);
                    return;
                }
            },
            Kind::GotAbs => match slot(class.slot) {
                Some(g) => g.wrapping_add_signed(a),
                None => {
                    self.report_apply(rel, ApplyError::BadInstruction);
                    return;
                }
            },
            Kind::GotSlotRel => match slot(class.slot) {
                Some(g) => g.wrapping_sub(addresses.got_base()).wrapping_add_signed(a),
                None => {
                    self.report_apply(rel, ApplyError::BadInstruction);
                    return;
                }
            },
            Kind::GotRel => sa.wrapping_sub(addresses.got_base()),
            Kind::GotBasePc => addresses
                .got_base()
                .wrapping_add_signed(a)
                .wrapping_sub(place),
            // An undefined (weak) TLS symbol has no offset; GNU ld and lld
            // write the addend.
            Kind::TpOff | Kind::DtpOff if undefined => a as u64,
            Kind::TpOff => sa.wrapping_sub(tp),
            // Arm has no local-dynamic relaxation: `__tls_get_addr` returns
            // the start of the module's block, whatever the output is.
            Kind::DtpOff => sa.wrapping_sub(tls.start),
            _ => {
                self.report_apply(rel, ApplyError::BadInstruction);
                return;
            }
        };
        self.put(out, rel, class.width, value);
    }

    /// Writes a branch: interworking, thunks and undefined weak targets.
    fn branch(
        &self,
        rel: &Relocation,
        out: &mut [u8],
        target: &Target,
        sa: u64,
        via_stub: bool,
        undefined: bool,
    ) {
        let super::Patch::Insn(field) = super::patch_of(rel.r_type) else {
            return;
        };
        let at = usize::try_from(rel.offset).unwrap_or(usize::MAX);
        let Some(insn) = field.read(out, at) else {
            self.report_apply(rel, ApplyError::OutOfBounds);
            return;
        };
        // A branch to an undefined weak symbol without a PLT entry has
        // nowhere to go: GNU ld writes a `nop`.
        if undefined && sa == 0 {
            let nop = if super::is_thumb_branch(rel.r_type) {
                insn::THUMB_NOP_W
            } else {
                (insn & 0xf000_0000) | insn::NOP
            };
            if field.write(out, at, nop).is_none() {
                self.report_apply(rel, ApplyError::OutOfBounds);
            }
            return;
        }
        let place = self.section.base.wrapping_add(rel.offset);
        let function = via_stub
            || target
                .raw
                .is_some_and(|raw| matches!(raw.kind(), STT_FUNC | STT_GNU_IFUNC));
        let branch = Branch {
            r_type: rel.r_type,
            insn,
            place,
            destination: sa.wrapping_add(super::pc_bias(rel.r_type)),
            function,
            via_stub,
        };
        let plan = branch.plan();
        let caller_thumb = branch.thumb_caller();
        let (destination, thumb_target) = if plan.thunk {
            let Some(address) = self.thunk_address(&branch) else {
                self.report_apply(rel, ApplyError::Overflow);
                return;
            };
            // The thunk is in the caller's state, and the branch reaches
            // it directly.
            (address, caller_thumb)
        } else {
            (branch.destination, plan.thumb_target)
        };
        let source = place.wrapping_add(super::pc_bias(rel.r_type));
        let (insn, value) = match (caller_thumb, thumb_target) {
            // A32 to A32.
            (false, false) => {
                let insn = if insn::is_arm_blx(insn) {
                    insn::arm_bl(insn)
                } else {
                    insn
                };
                (insn, destination.wrapping_sub(source))
            }
            // A32 to Thumb: `blx`, whose H bit takes bit 1 of the offset.
            (false, true) => {
                match insn::arm_blx(i64::from(destination.wrapping_sub(source) as u32 as i32)) {
                    Ok(word) => {
                        if field.write(out, at, word).is_none() {
                            self.report_apply(rel, ApplyError::OutOfBounds);
                        }
                        return;
                    }
                    Err(_) => {
                        self.report_apply(rel, ApplyError::Overflow);
                        return;
                    }
                }
            }
            // Thumb to Thumb.
            (true, true) => {
                let insn = if rel.r_type == super::R_ARM_THM_CALL {
                    insn::thumb_set_blx(insn, false)
                } else {
                    insn
                };
                (insn, (destination & !1).wrapping_sub(source))
            }
            // Thumb to A32: `blx`, which branches from the aligned PC.
            (true, false) => {
                let value = destination.wrapping_sub(source & !3);
                (insn::thumb_set_blx(insn, true), value.wrapping_add(3) & !3)
            }
        };
        let value = i64::from(value as u32 as i32);
        match field.encode(insn, value) {
            Ok(word) => {
                if field.write(out, at, word).is_none() {
                    self.report_apply(rel, ApplyError::OutOfBounds);
                }
            }
            Err(_) => self.report_apply(rel, ApplyError::Overflow),
        }
    }

    /// The address of the thunk `branch` goes through.
    fn thunk_address(&self, branch: &Branch) -> Option<u64> {
        let layout = self.addresses().layout;
        let key = super::thunks::key_of(branch, self.input.context.mode.pic)?;
        let shndx = layout
            .section_shndx
            .get(self.section.id.index())
            .copied()
            .unwrap_or(0);
        let output = layout.output_of_shndx(shndx)?;
        layout.thunk_for(output, key)
    }
}

/// Whether the writer handles section `header` itself, rather than
/// copying and relocating it: the merged `.ARM.attributes` and the one
/// exception index take the place of their first input section, and the
/// others are empty.
#[must_use]
pub fn is_replaced(sh_type: u32) -> bool {
    matches!(sh_type, SHT_ARM_ATTRIBUTES | SHT_ARM_EXIDX)
}

/// The size and alignment of Arm input section `id` in the output: the
/// merged `.ARM.attributes` or the whole exception index for the first
/// such section, nothing for the others.
#[must_use]
pub fn member_size(
    prepared: Option<&super::Prepared>,
    id: SectionId,
    sh_type: u32,
    size: u64,
    align: u64,
) -> (u64, u64) {
    match sh_type {
        SHT_ARM_ATTRIBUTES => match prepared.and_then(|p| p.attributes.as_ref()) {
            Some(merged) if merged.first == id => {
                (u64::try_from(merged.bytes.len()).unwrap_or(0), 1)
            }
            Some(_) => (0, 1),
            None => (size, align),
        },
        SHT_ARM_EXIDX => match prepared.and_then(|p| p.exidx.as_ref()) {
            Some(table) if table.first == id => (table.size, 4),
            Some(_) => (0, 4),
            None => (size, align),
        },
        _ => (size, align),
    }
}
