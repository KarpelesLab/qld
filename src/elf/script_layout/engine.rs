//! Address assignment under a linker script (pipeline stage 10b).
//!
//! The engine walks the statement list the way GNU ld's
//! `lang_size_sections` does: top-level assignments move `.` and define
//! symbols; each output section statement evaluates its address (or takes
//! the next free address of its memory region, aligned to the section),
//! lays out its input sections, data commands and assignments in order, and
//! gets a load address from `AT`, `AT>` or the previous section of its
//! region. `/DISCARD/` and statements dropped by a constraint are skipped.
//!
//! Symbol values may depend on addresses that are only known later in the
//! script, so passes repeat, as lld's `assignAddresses` does, until
//! every output section address and size and every script symbol value is
//! the same as in the previous pass. `DATA_SEGMENT_ALIGN`,
//! `DATA_SEGMENT_RELRO_END` and `DATA_SEGMENT_END` follow GNU ld's state
//! machine, including the extra pass that moves the RELRO region so that it
//! ends on a page boundary. Errors (undefined symbols in expressions, `.`
//! moving backwards, failed `ASSERT`s, region overflows) are reported from
//! the final pass only.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::args::MagicMode;
use crate::diag::Diagnostic;
use crate::elf::layout::{
    CompressedOutput, Layout, LayoutInput, Member, OutSection, Placed, Trailer, add_trailers,
    entsize_of, member_size, set_links, synthetic_flags,
};
use crate::elf::object::SectionKind;
use crate::elf::read::consts::{SHF_ALLOC, SHF_TLS, SHF_WRITE, SHT_NOBITS, SHT_NOTE, SHT_PROGBITS};
use crate::elf::rules::Synthetic;
use crate::elf::sections::NONE;
use crate::error::{Error, Result};
use crate::ids::SectionId;
use crate::script::{
    AssignKind, Assignment, EvalContext, EvalError, Expr, Fill, OutputSectionType, SortMode, Span,
    Value, ValueSection, align_up, eval, eval_absolute, eval_dot_assignment,
    eval_symbol_assignment, fill_pattern,
};

use super::matching::{ScriptPlacement, SymbolDef, gnu_flags, sec};
use super::plan::{Item, LayoutScript, OutputStmt, OverlayRole, Statement};

/// Most address assignment passes before giving up.
const MAX_PASSES: u32 = 12;

/// The final value of a linker script symbol.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptSymbol {
    /// The symbol's name.
    pub name: Vec<u8>,
    /// Its address or value.
    pub value: u64,
    /// Whether the value is absolute (`SHN_ABS`).
    pub absolute: bool,
    /// Whether an assignment defined it.
    pub defined: bool,
    /// For a section-relative value, the position in
    /// [`Layout::sections`] of its output section.
    pub section: Option<u32>,
}

/// One member of an output section during assignment.
#[derive(Clone, Copy, Debug)]
struct Entry {
    member: Member,
    sub: u16,
    size: u64,
    align: u64,
    offset: u64,
}

/// One step of an output section's program.
#[derive(Clone, Copy)]
enum Step<'s> {
    /// Members `start..end` of the output's entry list.
    Run(usize, usize),
    /// A script item.
    Item(&'s Item),
}

/// A memory region during assignment.
#[derive(Clone, Debug, Default)]
struct RegionState {
    origin: u64,
    length: u64,
    current: u64,
    last_os: Option<u32>,
    full_message: bool,
    attrs_flags: u8,
    attrs_not: u8,
}

/// An output section during assignment.
#[derive(Clone, Debug, Default)]
struct OutState {
    exists: bool,
    has_input: bool,
    vma: u64,
    lma: u64,
    size: u64,
    align: u64,
    processed: bool,
    /// `.` was assigned to non-trivially inside (GNU's `SEC_KEEP`).
    keep: bool,
    /// A symbol is assigned inside (GNU's `update_dot`).
    symbols_inside: bool,
    /// `.` moved inside, which makes the section allocated.
    dot_moved: bool,
    has_data: bool,
    region: Option<usize>,
    lma_region: Option<usize>,
    fills: Vec<(u64, u64, u32)>,
    data: Vec<(u64, Vec<u8>)>,
    /// GNU flags from the inputs.
    input_flags: u32,
}

impl OutState {
    fn emitted(&self, emit_relocs: bool) -> bool {
        self.exists && (self.size != 0 || self.keep || (emit_relocs && self.has_input))
    }

    fn ignored(&self, emit_relocs: bool) -> bool {
        !self.emitted(emit_relocs) && !self.symbols_inside
    }
}

/// `DATA_SEGMENT_*` phases, as GNU ld's `exp_seg_*`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SegPhase {
    #[default]
    None,
    AlignSeen,
    RelroSeen,
    EndSeen,
    RelroAdjust,
    Adjust,
    Done,
}

#[derive(Clone, Copy, Debug, Default)]
struct DataSeg {
    phase: SegPhase,
    base: u64,
    relro_end: u64,
    relro_offset: u64,
    end: u64,
    min_base: u64,
    max_page: u64,
    common_page: u64,
}

#[derive(Clone, Debug)]
struct SymState {
    value: Option<Value<u32>>,
    pass: u32,
    needed: bool,
}

/// What a pass records for the convergence check.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    outputs: Vec<(u64, u64, u64, bool)>,
    symbols: Vec<Option<(u64, bool)>>,
}

struct Engine<'e, 'l, 'a> {
    input: &'e LayoutInput<'l, 'a>,
    script: &'e LayoutScript,
    placed: &'e ScriptPlacement,
    entries: Vec<Vec<Entry>>,
    programs: Vec<Vec<Step<'e>>>,
    outs: Vec<OutState>,
    regions: Vec<RegionState>,
    default_region: usize,
    syms: Vec<SymState>,
    sym_index: HashMap<&'e [u8], usize, foldhash::fast::FixedState>,
    defs: HashMap<&'e [u8], SymbolDef, foldhash::fast::FixedState>,
    /// Offset in its output of each input section member.
    member_offset: Vec<u64>,
    merge_offset: Vec<u64>,
    dot: u64,
    current: Option<u32>,
    last_os: Option<u32>,
    /// GNU's `prefer_next_section`: `.` was assigned outside an output
    /// section, so symbols assigned from `.` belong to the next section.
    prefer_next: bool,
    /// GNU's `found_end`: an `_end`-like symbol has been assigned, after
    /// which symbols always belong to the previous section.
    found_end: bool,
    pass: u32,
    final_pass: bool,
    errors: Vec<(Span, String)>,
    warnings: Vec<String>,
    dataseg: DataSeg,
    fill_patterns: Vec<Vec<u8>>,
    headers_size: u64,
    /// Whether `headers_size` is GNU ld's first estimate, which the caller
    /// corrects by laying out again when the headers outgrow it.
    headers_estimated: bool,
    max_page: u64,
    common_page: u64,
    /// `-z relro`, dropped as GNU ld does when no section with contents
    /// lies between `DATA_SEGMENT_ALIGN` and `DATA_SEGMENT_RELRO_END`.
    relro: bool,
    span: Span,
    /// Output statements by name, first enabled one.
    by_name: HashMap<&'e [u8], u32, foldhash::fast::FixedState>,
    /// The load region of each output, after GNU's
    /// `lang_propagate_lma_regions`.
    lma_regions: Vec<Option<usize>>,
}

fn hasher() -> foldhash::fast::FixedState {
    foldhash::fast::FixedState::with_seed(0x7363_7269)
}

/// GNU's `is_value (…, 0)`, `is_dot_plus_0` and `is_align_conditional`:
/// assignments to `.` that do not keep an otherwise empty section.
fn is_trivial_dot_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Number(0) => true,
        Expr::Binary(crate::script::BinaryOp::Add, lhs, rhs) => {
            matches!(**lhs, Expr::Dot) && matches!(**rhs, Expr::Number(0))
        }
        Expr::Align(inner) => matches!(
            &**inner,
            Expr::Conditional(cond, _, otherwise)
                if matches!(**otherwise, Expr::Number(1))
                    && matches!(&**cond, Expr::Binary(crate::script::BinaryOp::Ne, l, r)
                        if matches!(**l, Expr::Dot) && matches!(**r, Expr::Number(0)))
        ),
        _ => false,
    }
}

impl<'e, 'l, 'a> Engine<'e, 'l, 'a> {
    fn stmt(&self, index: u32) -> Option<&'e OutputStmt> {
        self.placed.stmt(self.script, index)
    }

    fn enabled(&self, index: u32) -> bool {
        self.placed
            .enabled
            .get(index as usize)
            .copied()
            .unwrap_or(false)
            && self.stmt(index).is_some_and(|s| !s.is_discard())
    }

    fn emit_relocs(&self) -> bool {
        self.input.options.emit_relocs
    }

    fn record_error(&mut self, span: Span, message: impl Into<String>) {
        if self.final_pass {
            self.errors.push((span, message.into()));
        }
    }

    fn alloc_of(&self, index: u32) -> bool {
        let out = match self.outs.get(index as usize) {
            Some(o) => o,
            None => return false,
        };
        let stmt_noalloc = self.stmt(index).is_some_and(OutputStmt::is_noalloc_type);
        !stmt_noalloc && (out.input_flags & sec::ALLOC != 0 || out.dot_moved || out.has_data)
    }

    /// Whether the output section holds thread-local data, which GNU ld
    /// never associates a symbol with.
    fn is_tls(&self, index: u32) -> bool {
        self.input
            .placement
            .outputs
            .get(index as usize)
            .is_some_and(|o| o.flags & SHF_TLS != 0)
    }

    fn is_tbss(&self, index: u32) -> bool {
        let Some(output) = self.input.placement.outputs.get(index as usize) else {
            return false;
        };
        output.flags & SHF_TLS != 0 && output.sh_type == SHT_NOBITS
    }

    /// GNU's `lang_memory_default`.
    fn default_region_for(&self, index: u32) -> usize {
        let mut flags = self.outs.get(index as usize).map_or(0, |o| o.input_flags);
        if flags & (sec::ALLOC | sec::READONLY | sec::CODE) == sec::ALLOC {
            flags |= sec::DATA;
        }
        // Region attribute bits: R=1 W=2 X=4 A=8 L=16.
        let mut region_bits = 0u8;
        if flags & sec::READONLY != 0 {
            region_bits |= 1;
        }
        if flags & sec::DATA != 0 {
            region_bits |= 2;
        }
        if flags & sec::CODE != 0 {
            region_bits |= 4;
        }
        if flags & sec::ALLOC != 0 {
            region_bits |= 8;
        }
        if flags & sec::LOAD != 0 {
            region_bits |= 16;
        }
        self.regions
            .iter()
            .take(self.default_region)
            .position(|r| r.attrs_flags & region_bits != 0 && r.attrs_not & region_bits == 0)
            .unwrap_or(self.default_region)
    }

    fn region_named(&self, name: &[u8]) -> Option<usize> {
        self.script.region_index(name)
    }

    fn fill_index(&mut self, pattern: Vec<u8>) -> u32 {
        if let Some(at) = self.fill_patterns.iter().position(|p| *p == pattern) {
            return u32::try_from(at).unwrap_or(0);
        }
        self.fill_patterns.push(pattern);
        u32::try_from(self.fill_patterns.len().saturating_sub(1)).unwrap_or(0)
    }

    /// The address of member `entry` within its section, as a symbol sees
    /// it.
    fn object_symbol(&self, file: usize, section: u32, value: u64) -> Option<Value<u32>> {
        let sections = self.input.sections;
        let mut id = sections.id(file, section)?;
        if !sections.is_live(id) {
            id = sections.resolve(id)?;
        }
        let output = self.input.placement.output_of(id)?;
        let object = self
            .input
            .files
            .get(sections.locate(id)?.0)?
            .object
            .as_ref()?;
        let input_section = object.section(sections.locate(id)?.1)?;
        let offset = match input_section.kind {
            SectionKind::Merge => {
                let group = self.input.merged.group_of(id)?;
                let base = *self.merge_offset.get(group as usize)?;
                let size = input_section.header.sh_size;
                if value == size && size != 0 {
                    let last = self
                        .input
                        .merged
                        .offset_in_group(id, value.checked_sub(1)?)?;
                    base.checked_add(last)?.checked_add(1)?
                } else {
                    base.checked_add(self.input.merged.offset_in_group(id, value)?)?
                }
            }
            SectionKind::EhFrame => {
                let start = *self.member_offset.get(id.index())?;
                let eh = self
                    .input
                    .eh_frames
                    .sections
                    .get(self.input.eh_frames.find(id)?)?;
                let offset32 = u32::try_from(value).ok()?;
                eh.records
                    .iter()
                    .find(|r| {
                        r.live && offset32 >= r.offset && offset32.wrapping_sub(r.offset) < r.size
                    })
                    .map_or(start, |r| {
                        start.wrapping_add(u64::from(
                            r.out_offset.wrapping_add(offset32.wrapping_sub(r.offset)),
                        ))
                    })
            }
            _ => self.member_offset.get(id.index())?.checked_add(value)?,
        };
        Some(Value::relative(output, offset))
    }

    /// The output of a named linker-generated symbol such as `__start_X`.
    fn linker_symbol(&self, name: &[u8]) -> Option<Value<u32>> {
        let start = name.strip_prefix(b"__start_");
        let stop = name.strip_prefix(b"__stop_");
        if let Some(section) = start.or(stop) {
            let index = *self.by_name.get(section)?;
            let size = self.outs.get(index as usize)?.size;
            return Some(Value::relative(
                index,
                if stop.is_some() { size } else { 0 },
            ));
        }
        None
    }

    /// GNU's `section_for_dot`: the output section a symbol assigned from
    /// `.` outside an output section belongs to. Assignments belong to the
    /// previous section, unless `.` has been assigned since it ended, in
    /// which case they belong to the next one; past an `_end`-like symbol
    /// they always belong to the previous section.
    fn section_for_dot(&self) -> Option<u32> {
        if (self.last_os.is_none() || (self.prefer_next && !self.found_end))
            && let Some(next) = self.next_alloc_output()
        {
            return Some(next);
        }
        self.last_os
    }

    /// The first allocated output section that exists after the one being
    /// processed, in statement order.
    fn next_alloc_output(&self) -> Option<u32> {
        let statements = &self.placed.statements;
        let start = match self.last_os {
            Some(last) => statements
                .iter()
                .position(|s| matches!(s, Statement::Output(i) if *i == last))
                .map(|p| p.saturating_add(1))?,
            None => 0,
        };
        statements.get(start..)?.iter().find_map(|s| match s {
            Statement::Output(i)
                if self.enabled(*i)
                    && self.outs.get(*i as usize).is_some_and(|o| o.exists)
                    && self.alloc_of(*i)
                    && !self.is_tls(*i) =>
            {
                Some(*i)
            }
            _ => None,
        })
    }

    fn output_by_name(&self, name: &[u8]) -> Option<u32> {
        if name == b"NEXT_SECTION" {
            let statements = &self.placed.statements;
            let position = self.last_os.and_then(|last| {
                statements
                    .iter()
                    .position(|s| matches!(s, Statement::Output(i) if *i == last))
            })?;
            return statements
                .get(position.saturating_add(1)..)?
                .iter()
                .find_map(|s| match s {
                    Statement::Output(i)
                        if self.enabled(*i)
                            && self.outs.get(*i as usize).is_some_and(|o| o.exists) =>
                    {
                        Some(*i)
                    }
                    _ => None,
                });
        }
        self.by_name.get(name).copied()
    }

    // ---- statements ----

    fn assignment(&mut self, assignment: &'e Assignment, span: Span) {
        self.span = span;
        let target = assignment.target.as_slice();
        if target
            .iter()
            .copied()
            .skip_while(|&b| b == b'_')
            .eq(*b"end")
        {
            self.found_end = true;
        }
        if assignment.is_dot() {
            if self.current.is_none() {
                self.prefer_next = true;
            }
            match eval_dot_assignment(assignment, self) {
                Ok(next) => {
                    self.dot = next;
                    if self.current.is_none()
                        && let Some(region) = self.regions.get_mut(self.default_region)
                    {
                        region.current = next;
                    }
                }
                Err(error) => self.record_error(span, error.to_string()),
            }
            return;
        }
        let Some(&slot) = self.sym_index.get(assignment.target.as_slice()) else {
            return;
        };
        let provide = matches!(
            assignment.kind,
            AssignKind::Provide | AssignKind::ProvideHidden
        );
        if provide && !self.syms.get(slot).is_some_and(|s| s.needed) {
            return;
        }
        match eval_symbol_assignment(assignment, self) {
            Ok(mut value) => {
                // GNU's rel_from_abs: `sym = .` outside sections becomes
                // relative to the section holding `.` once it is final.
                if value.from_dot
                    && value.section == ValueSection::Absolute
                    && let Some(index) = self.section_for_dot()
                    && let Some(out) = self.outs.get(index as usize)
                {
                    value = Value::relative(index, value.value.wrapping_sub(out.vma));
                }
                let pass = self.pass;
                if let Some(sym) = self.syms.get_mut(slot) {
                    sym.value = Some(value);
                    sym.pass = pass;
                }
            }
            Err(error) => self.record_error(span, error.to_string()),
        }
    }

    fn assert(&mut self, expr: &Expr, message: &[u8], span: Span) {
        self.span = span;
        match eval(expr, self) {
            Ok(value) => {
                if value.value == 0 && self.final_pass {
                    self.errors
                        .push((span, String::from_utf8_lossy(message).into_owned()));
                }
            }
            Err(error) => self.record_error(span, error.to_string()),
        }
    }

    fn eval_abs(&mut self, expr: &Expr, span: Span) -> Option<u64> {
        self.span = span;
        match eval_absolute(expr, self) {
            Ok(v) => Some(v),
            Err(error) => {
                self.record_error(span, error.to_string());
                None
            }
        }
    }

    /// Pads the current section from `from` to `self.dot` with `fill`.
    fn pad(&mut self, index: u32, from: u64, fill: Option<u32>) {
        let Some(fill) = fill else {
            return;
        };
        if !self.final_pass || self.dot <= from {
            return;
        }
        let vma = self.outs.get(index as usize).map_or(0, |o| o.vma);
        let offset = from.wrapping_sub(vma);
        let size = self.dot.wrapping_sub(from);
        if let Some(out) = self.outs.get_mut(index as usize) {
            out.fills.push((offset, size, fill));
        }
    }

    #[allow(clippy::too_many_lines)]
    fn output(&mut self, index: u32) {
        let Some(stmt) = self.stmt(index) else {
            return;
        };
        if !self.enabled(index) {
            return;
        }
        self.span = stmt.span;
        let explicit_start = self
            .input
            .options
            .section_starts
            .iter()
            .rev()
            .find(|(name, _)| name.as_bytes() == stmt.name.as_slice())
            .map(|(_, address)| *address);
        let has_address = explicit_start.is_some() || stmt.address.is_some();
        // In GNU ld's final assignment pass, which gives symbols their
        // values, sections that are not output leave `.` alone.
        let dot_before = self.dot;
        if let Some(address) = explicit_start {
            self.dot = address;
        } else if let Some(expr) = &stmt.address {
            self.current = None;
            if let Some(address) = self.eval_abs(expr, stmt.span) {
                self.dot = address;
            }
        }
        let exists = self.outs.get(index as usize).is_some_and(|o| o.exists);
        if !exists {
            self.dot = dot_before;
            return;
        }
        let emit_relocs = self.emit_relocs();
        let ignored = self
            .outs
            .get(index as usize)
            .is_some_and(|o| o.ignored(emit_relocs));
        let input_align = self.entries.get(index as usize).map_or(1, |entries| {
            entries.iter().map(|e| e.align).max().unwrap_or(1)
        });
        let attr_align = match &stmt.align {
            Some(expr) => self.eval_abs(expr, stmt.span).unwrap_or(1),
            None => 1,
        };
        let subalign = match &stmt.subalign {
            Some(expr) => self.eval_abs(expr, stmt.span),
            None => None,
        };
        let member_align_max = subalign.map_or(input_align, |s| s.max(1));
        let section_align = member_align_max.max(attr_align).max(1);

        let alloc = self.alloc_of(index);
        let region;
        let mut newdot = self.dot;
        let mut used_align = attr_align;
        if !has_address {
            let explicit = stmt.region.as_deref().and_then(|n| self.region_named(n));
            let r = match explicit {
                Some(r) => r,
                None => self.default_region_for(index),
            };
            if r == self.default_region
                && self.default_region > 0
                && alloc
                && !self.is_tbss(index)
                && !ignored
                && self.final_pass
            {
                let name = String::from_utf8_lossy(&stmt.name).into_owned();
                self.errors.push((
                    Span::default(),
                    format!("error: no memory region specified for loadable section `{name}'"),
                ));
            }
            region = Some(r);
            newdot = self.regions.get(r).map_or(self.dot, |reg| reg.current);
            used_align = section_align;
        } else {
            region = Some(
                stmt.region
                    .as_deref()
                    .and_then(|n| self.region_named(n))
                    .unwrap_or(self.default_region),
            );
        }
        let before_align = newdot;
        newdot = align_up(newdot, used_align);
        let dotdelta_align = newdot.wrapping_sub(before_align);
        let vma = newdot;

        // Children.
        let fill_default = stmt.fill.as_ref();
        let mut fill: Option<u32> = None;
        if let Some(f) = fill_default {
            fill = self.fill_value(f, index);
        }
        if let Some(out) = self.outs.get_mut(index as usize) {
            out.vma = vma;
            out.align = section_align;
            out.fills.clear();
            out.data.clear();
            out.processed = true;
            out.region = region;
        }
        self.dot = vma;
        let previous_current = self.current;
        self.current = Some(index);
        let program = self
            .programs
            .get(index as usize)
            .cloned()
            .unwrap_or_default();
        for step in program {
            match step {
                Step::Run(start, end) => {
                    for i in start..end {
                        let Some(entry) = self
                            .entries
                            .get(index as usize)
                            .and_then(|e| e.get(i))
                            .copied()
                        else {
                            continue;
                        };
                        let align = subalign.map_or(entry.align, |s| s.max(1));
                        let from = self.dot;
                        let aligned = align_up(self.dot, align);
                        self.dot = aligned;
                        self.pad(index, from, fill);
                        let offset = aligned.wrapping_sub(vma);
                        let size = entry.size;
                        let member = entry.member;
                        if let Some(entry) = self
                            .entries
                            .get_mut(index as usize)
                            .and_then(|e| e.get_mut(i))
                        {
                            entry.offset = offset;
                        }
                        match member {
                            Member::Input(id) => {
                                if let Some(slot) = self.member_offset.get_mut(id.index()) {
                                    *slot = offset;
                                }
                            }
                            Member::Merge(group) => {
                                if let Some(slot) = self.merge_offset.get_mut(group as usize) {
                                    *slot = offset;
                                }
                            }
                            Member::Synthetic(_) => {}
                        }
                        self.dot = self.dot.wrapping_add(size);
                    }
                }
                Step::Item(item) => match item {
                    Item::Input { .. } => {}
                    Item::Assign { assignment, span } => {
                        let from = self.dot;
                        self.assignment(assignment, *span);
                        if self.dot != from {
                            if let Some(out) = self.outs.get_mut(index as usize) {
                                out.dot_moved = true;
                            }
                            self.pad(index, from, fill);
                        }
                    }
                    Item::Data { size, expr, span } => {
                        self.span = *span;
                        let offset = self.dot.wrapping_sub(vma);
                        let value = match eval(expr, self) {
                            Ok(v) => v.resolve(self),
                            Err(error) => {
                                self.record_error(*span, error.to_string());
                                0
                            }
                        };
                        let bytes = value.to_le_bytes();
                        let width = usize::try_from(size.bytes()).unwrap_or(8).min(8);
                        let data = bytes.get(..width).unwrap_or(&bytes).to_vec();
                        if let Some(out) = self.outs.get_mut(index as usize) {
                            out.has_data = true;
                            if self.final_pass {
                                out.data.push((offset, data));
                            }
                        }
                        self.dot = self.dot.wrapping_add(size.bytes());
                    }
                    Item::Asciz(text) => {
                        let offset = self.dot.wrapping_sub(vma);
                        let mut bytes = text.clone();
                        bytes.push(0);
                        let len = u64::try_from(bytes.len()).unwrap_or(0);
                        if let Some(out) = self.outs.get_mut(index as usize) {
                            out.has_data = true;
                            if self.final_pass {
                                out.data.push((offset, bytes));
                            }
                        }
                        self.dot = self.dot.wrapping_add(len);
                    }
                    Item::LinkerVersion => {
                        let offset = self.dot.wrapping_sub(vma);
                        let bytes = crate::elf::synth::comment();
                        let len = u64::try_from(bytes.len()).unwrap_or(0);
                        if let Some(out) = self.outs.get_mut(index as usize) {
                            out.has_data = true;
                            if self.final_pass {
                                out.data.push((offset, bytes));
                            }
                        }
                        self.dot = self.dot.wrapping_add(len);
                    }
                    Item::Fill(f) => {
                        fill = self.fill_value(f, index);
                    }
                    Item::Assert { assert, span } => {
                        self.assert(&assert.expr, &assert.message, *span);
                    }
                },
            }
        }
        self.current = previous_current;
        let size = self.dot.wrapping_sub(vma);
        if let Some(out) = self.outs.get_mut(index as usize) {
            out.size = size;
        }
        self.dot = vma;

        // Load address.
        let r = region.unwrap_or(self.default_region);
        let lma_region = self.lma_regions.get(index as usize).copied().flatten();
        let mut lma = vma;
        if let Some(expr) = &stmt.load_address {
            if let Some(value) = self.eval_abs(expr, stmt.span) {
                lma = value;
            }
        } else if let Some(lr) = lma_region {
            let mut value = self.regions.get(lr).map_or(0, |reg| reg.current);
            if stmt.align_with_input {
                value = value.wrapping_add(dotdelta_align);
            } else {
                let lalign = if Some(lr) != region {
                    attr_align
                } else {
                    used_align
                };
                value = align_up(value, lalign);
            }
            lma = value;
        } else if let Some(last) = self.regions.get(r).and_then(|reg| reg.last_os)
            && alloc
        {
            let (last_vma, last_lma, last_size) = self
                .outs
                .get(last as usize)
                .map_or((0, 0, 0), |o| (o.vma, o.lma, o.size));
            if self.dot < last_vma && size != 0 && self.dot.wrapping_add(size) <= last_vma {
                if last_vma != last_lma && self.final_pass {
                    self.warnings.push(format!(
                        "dot moved backwards before `{}'",
                        String::from_utf8_lossy(&stmt.name)
                    ));
                }
            } else {
                lma = if stmt.overlay == OverlayRole::Rest {
                    last_lma.wrapping_add(last_size)
                } else {
                    vma.wrapping_add(last_lma).wrapping_sub(last_vma)
                };
                lma = align_up(lma, used_align);
            }
        }
        if let Some(out) = self.outs.get_mut(index as usize) {
            out.lma = lma;
            out.lma_region = lma_region;
        }
        let track = {
            let ignore_section = !alloc || self.is_tbss(index);
            let last = self.regions.get(r).and_then(|reg| reg.last_os);
            let last_vma = last
                .and_then(|l| self.outs.get(l as usize))
                .map_or(0, |o| o.vma);
            ((!ignore_section
                && (size != 0
                    || (last.is_none() && vma != lma)
                    || (last.is_some() && self.dot >= last_vma)))
                || stmt.overlay == OverlayRole::First)
                && lma_region.is_none()
        };
        if track && let Some(reg) = self.regions.get_mut(r) {
            reg.last_os = Some(index);
        }
        if ignored {
            self.dot = dot_before;
            return;
        }
        if alloc {
            self.last_os = Some(index);
            self.prefer_next = false;
        }
        let dotdelta = if self.is_tbss(index) { 0 } else { size };
        self.dot = self.dot.wrapping_add(dotdelta);
        if let Some(expr) = &stmt.update_dot {
            self.current = None;
            if let Some(value) = self.eval_abs(expr, stmt.span) {
                self.dot = self.dot.max(value);
            }
        }
        if let Some(region) = region
            && alloc
        {
            if let Some(reg) = self.regions.get_mut(region) {
                reg.current = self.dot;
            }
            self.check_region(region, index, has_address, vma);
            if let Some(lr) = lma_region
                && Some(lr) != Some(region)
            {
                let loads = self
                    .outs
                    .get(index as usize)
                    .is_some_and(|o| o.input_flags & sec::LOAD != 0 || o.has_data);
                if loads || stmt.align_with_input {
                    if let Some(reg) = self.regions.get_mut(lr) {
                        reg.current = lma.wrapping_add(dotdelta);
                    }
                    self.check_region(lr, index, false, lma);
                }
            }
        }
    }

    fn fill_value(&mut self, fill: &Fill, index: u32) -> Option<u32> {
        let previous = self.current;
        self.current = Some(index);
        let pattern = match fill_pattern(fill, self) {
            Ok(p) => p,
            Err(error) => {
                let span = self.span;
                self.record_error(span, error.to_string());
                Vec::new()
            }
        };
        self.current = previous;
        if pattern.is_empty() {
            return None;
        }
        Some(self.fill_index(pattern))
    }

    /// GNU's `os_region_check`.
    fn check_region(&mut self, region: usize, index: u32, explicit: bool, base: u64) {
        if region == self.default_region || !self.final_pass {
            return;
        }
        let Some(reg) = self.regions.get(region) else {
            return;
        };
        let end = reg.origin.wrapping_add(reg.length);
        let outside = (reg.current < reg.origin
            || reg.current.wrapping_sub(reg.origin) > reg.length)
            && (reg.current != end || base == 0);
        if !outside {
            return;
        }
        let name = String::from_utf8_lossy(
            self.script
                .regions
                .get(region)
                .map_or(&[][..], |r| r.name.as_slice()),
        )
        .into_owned();
        let section =
            String::from_utf8_lossy(self.stmt(index).map_or(&[][..], |s| &s.name)).into_owned();
        let output = self.input.options.output_path().display().to_string();
        let span = self.stmt(index).map_or_else(Span::default, |s| s.span);
        if explicit {
            let current = reg.current;
            self.errors.push((
                span,
                format!(
                    "address {current:#x} of {output} section `{section}' is not within region `{name}'"
                ),
            ));
        } else if !reg.full_message {
            self.errors.push((
                span,
                format!("{output} section `{section}' will not fit in region `{name}'"),
            ));
            if let Some(reg) = self.regions.get_mut(region) {
                reg.full_message = true;
            }
        }
    }

    fn statement(&mut self, statement: &'e Statement) {
        match statement {
            Statement::Assign { assignment, span } => {
                self.current = None;
                self.assignment(assignment, *span);
            }
            Statement::Assert { assert, span } => {
                self.current = None;
                self.assert(&assert.expr, &assert.message, *span);
            }
            Statement::Output(index) => {
                self.current = None;
                self.output(*index);
                self.current = None;
            }
        }
    }

    fn reset_pass(&mut self) -> Result<()> {
        self.dot = 0;
        self.current = None;
        self.last_os = None;
        self.prefer_next = false;
        self.found_end = false;
        self.pass = self.pass.wrapping_add(1);
        for out in &mut self.outs {
            out.processed = false;
        }
        for region in &mut self.regions {
            region.current = region.origin;
            region.last_os = None;
            region.full_message = false;
        }
        Ok(())
    }

    fn run_pass(&mut self, final_pass: bool) -> Result<()> {
        self.reset_pass()?;
        self.final_pass = final_pass;
        let statements = &self.placed.statements;
        for statement in statements {
            self.statement(statement);
        }
        Ok(())
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            outputs: self
                .outs
                .iter()
                .map(|o| (o.vma, o.lma, o.size, o.keep || o.symbols_inside))
                .collect(),
            symbols: self
                .syms
                .iter()
                .map(|s| {
                    s.value.map(|v| {
                        (
                            v.resolve(self),
                            matches!(v.section, ValueSection::Absolute | ValueSection::Number),
                        )
                    })
                })
                .collect(),
        }
    }
}

impl EvalContext for Engine<'_, '_, '_> {
    type Section = u32;

    fn section_vma(&self, section: u32) -> u64 {
        self.outs.get(section as usize).map_or(0, |o| o.vma)
    }

    fn current_section(&self) -> Option<u32> {
        self.current
    }

    fn dot(&self) -> Result<u64, EvalError> {
        Ok(self.dot)
    }

    fn sane_expr(&self) -> bool {
        self.script.sane_expr
    }

    fn symbol(&mut self, name: &[u8]) -> Result<Value<u32>, EvalError> {
        if let Some(&slot) = self.sym_index.get(name)
            && let Some(value) = self.syms.get(slot).and_then(|s| s.value)
        {
            return Ok(value);
        }
        match self.defs.get(name).copied() {
            Some(SymbolDef::Section {
                file,
                section,
                value,
            }) => match self.object_symbol(file, section, value) {
                Some(v) => Ok(v),
                None if self.final_pass => Err(EvalError::Other(format!(
                    "unresolvable symbol `{}' referenced in expression",
                    String::from_utf8_lossy(name)
                ))),
                None => Ok(Value::absolute(0)),
            },
            Some(SymbolDef::Absolute(value)) => Ok(Value::absolute(value)),
            Some(SymbolDef::Linker) => Ok(self
                .linker_symbol(name)
                .unwrap_or_else(|| Value::absolute(0))),
            Some(SymbolDef::Other) => Ok(Value::absolute(0)),
            Some(SymbolDef::Undefined) | None => {
                if self.final_pass {
                    Err(EvalError::UndefinedSymbol(name.to_vec()))
                } else {
                    Ok(Value::absolute(0))
                }
            }
        }
    }

    fn is_defined(&mut self, name: &[u8]) -> bool {
        if let Some(&slot) = self.sym_index.get(name)
            && self
                .syms
                .get(slot)
                .is_some_and(|s| s.value.is_some() && s.pass == self.pass)
        {
            return true;
        }
        matches!(
            self.defs.get(name),
            Some(
                SymbolDef::Section { .. }
                    | SymbolDef::Absolute(_)
                    | SymbolDef::Linker
                    | SymbolDef::Other
            )
        )
    }

    fn section_addr(&mut self, name: &[u8]) -> Result<Value<u32>, EvalError> {
        match self.output_by_name(name) {
            Some(index) => Ok(Value::relative(index, 0)),
            None => Err(EvalError::UndefinedSection(name.to_vec())),
        }
    }

    fn section_load_addr(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        match self.output_by_name(name) {
            Some(index) => Ok(self.outs.get(index as usize).map_or(0, |o| o.lma)),
            None => Err(EvalError::UndefinedSection(name.to_vec())),
        }
    }

    fn section_size(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        match self.output_by_name(name) {
            Some(index) => Ok(self
                .outs
                .get(index as usize)
                .filter(|o| o.exists)
                .map_or(0, |o| o.size)),
            None if name == b"NEXT_SECTION" => Ok(0),
            None if self.final_pass => Err(EvalError::UndefinedSection(name.to_vec())),
            None => Ok(0),
        }
    }

    fn section_alignment(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        match self.output_by_name(name) {
            Some(index) => Ok(self
                .outs
                .get(index as usize)
                .filter(|o| o.exists)
                .map_or(0, |o| o.align.max(1))),
            None if name == b"NEXT_SECTION" => Ok(1),
            None if self.final_pass => Err(EvalError::UndefinedSection(name.to_vec())),
            None => Ok(0),
        }
    }

    fn region_origin(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        self.region_named(name)
            .and_then(|r| self.regions.get(r))
            .map(|r| r.origin)
            .ok_or_else(|| EvalError::UndefinedRegion(name.to_vec()))
    }

    fn region_length(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        self.region_named(name)
            .and_then(|r| self.regions.get(r))
            .map(|r| r.length)
            .ok_or_else(|| EvalError::UndefinedRegion(name.to_vec()))
    }

    fn sizeof_headers(&mut self) -> Result<u64, EvalError> {
        Ok(self.headers_size)
    }

    fn max_page_size(&self) -> Result<u64, EvalError> {
        Ok(self.max_page)
    }

    fn common_page_size(&self) -> Result<u64, EvalError> {
        Ok(self.common_page)
    }

    fn segment_start(&mut self, name: &[u8], default: u64) -> u64 {
        let options = self.input.options;
        match name {
            b"text-segment" => options.text_segment.or(options.image_base),
            b"rodata-segment" => options.rodata_segment,
            b"ldata-segment" => options.ldata_segment,
            _ => None,
        }
        .unwrap_or(default)
    }

    fn data_segment_align(
        &mut self,
        max_page_size: u64,
        common_page_size: u64,
        dot: u64,
    ) -> Result<u64, EvalError> {
        if self.current.is_some() {
            return Err(EvalError::Other(
                "DATA_SEGMENT_ALIGN used inside an output section".into(),
            ));
        }
        let seg = &mut self.dataseg;
        let mut value = align_up(dot, max_page_size);
        match seg.phase {
            SegPhase::RelroAdjust => value = seg.base,
            SegPhase::Adjust => {
                if common_page_size < max_page_size {
                    value = value.wrapping_add(
                        dot.wrapping_add(common_page_size).wrapping_sub(1)
                            & max_page_size.wrapping_sub(common_page_size),
                    );
                }
            }
            _ => {
                if !self.relro {
                    value = value.wrapping_add(dot & max_page_size.wrapping_sub(1));
                }
                if seg.phase == SegPhase::None {
                    seg.phase = SegPhase::AlignSeen;
                    seg.base = value;
                    seg.min_base = align_up(dot, max_page_size);
                    seg.common_page = common_page_size;
                    seg.max_page = max_page_size;
                    seg.relro_end = 0;
                }
            }
        }
        Ok(value)
    }

    fn data_segment_relro_end(&mut self, offset: u64, value: u64) -> Result<u64, EvalError> {
        let seg = &mut self.dataseg;
        seg.relro_offset = offset;
        match seg.phase {
            SegPhase::AlignSeen | SegPhase::Adjust | SegPhase::RelroAdjust | SegPhase::Done => {
                if matches!(seg.phase, SegPhase::AlignSeen | SegPhase::RelroAdjust) {
                    seg.relro_end = value.wrapping_add(offset);
                }
                let page = seg.max_page.max(1);
                let result = if seg.phase == SegPhase::RelroAdjust
                    && seg.relro_end & page.wrapping_sub(1) != 0
                {
                    seg.relro_end = align_up(seg.relro_end, page);
                    seg.relro_end.wrapping_sub(offset)
                } else {
                    value
                };
                if seg.phase == SegPhase::AlignSeen {
                    seg.phase = SegPhase::RelroSeen;
                }
                Ok(result)
            }
            _ => Ok(value),
        }
    }

    fn data_segment_end(&mut self, value: u64) -> Result<u64, EvalError> {
        let seg = &mut self.dataseg;
        if matches!(seg.phase, SegPhase::AlignSeen | SegPhase::RelroSeen) {
            seg.phase = SegPhase::EndSeen;
            seg.end = value;
        }
        Ok(value)
    }

    fn assertion_failed(&mut self, message: &[u8]) -> Result<(), EvalError> {
        if self.final_pass {
            let span = self.span;
            self.errors
                .push((span, String::from_utf8_lossy(message).into_owned()));
        }
        Ok(())
    }
}

/// Calls `f` on every node of an expression.
fn walk_expr(expr: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(expr);
    match expr {
        Expr::Unary(_, a)
        | Expr::Absolute(a)
        | Expr::Align(a)
        | Expr::Block(a)
        | Expr::DataSegmentEnd(a)
        | Expr::Log2Ceil(a)
        | Expr::Next(a)
        | Expr::SegmentStart(_, a)
        | Expr::Assert(a, _) => walk_expr(a, f),
        Expr::Binary(_, a, b)
        | Expr::AlignExpr(a, b)
        | Expr::DataSegmentAlign(a, b)
        | Expr::DataSegmentRelroEnd(a, b)
        | Expr::Max(a, b)
        | Expr::Min(a, b) => {
            walk_expr(a, f);
            walk_expr(b, f);
        }
        Expr::Conditional(c, a, b) => {
            walk_expr(c, f);
            walk_expr(a, f);
            walk_expr(b, f);
        }
        _ => {}
    }
}

/// Calls `f` on every expression of the statements.
fn for_each_expr(script: &LayoutScript, placed: &ScriptPlacement, f: &mut dyn FnMut(&Expr)) {
    for statement in &placed.statements {
        match statement {
            Statement::Assign { assignment, .. } => walk_expr(&assignment.expr, f),
            Statement::Assert { assert, .. } => walk_expr(&assert.expr, f),
            Statement::Output(index) => {
                let Some(stmt) = placed.stmt(script, *index) else {
                    continue;
                };
                for expr in [
                    &stmt.address,
                    &stmt.load_address,
                    &stmt.align,
                    &stmt.subalign,
                    &stmt.update_dot,
                ]
                .into_iter()
                .flatten()
                {
                    walk_expr(expr, f);
                }
                for item in &stmt.items {
                    match item {
                        Item::Assign { assignment, .. } => walk_expr(&assignment.expr, f),
                        Item::Data { expr, .. } => walk_expr(expr, f),
                        Item::Assert { assert, .. } => walk_expr(&assert.expr, f),
                        _ => {}
                    }
                }
            }
        }
    }
}

/// GNU ld's `ldlang_nearby_section`: the output section (by position) a
/// symbol defined in output `removed`, which is not output (empty and not
/// kept), moves to: the kept neighbour that would share its segment,
/// looking in the same memory region first.
fn nearby_section(
    engine: &Engine<'_, '_, '_>,
    output_places: &[(u64, u64, u32)],
    sections: &[OutSection<'_>],
    removed: u32,
    address: u64,
) -> Option<u32> {
    let statements = &engine.placed.statements;
    let at = statements
        .iter()
        .position(|s| matches!(s, Statement::Output(i) if *i == removed))?;
    let region_of = |output: u32| engine.outs.get(output as usize).and_then(|o| o.region);
    let region = region_of(removed);
    let kept = |statement: &Statement, same_region: bool| match statement {
        Statement::Output(i) => {
            let position = output_places.get(*i as usize).map(|p| p.2)?;
            (position != NONE && (!same_region || region_of(*i) == region)).then_some(position)
        }
        _ => None,
    };
    // GNU section flags of an emitted section, and of the removed one
    // (which never got SEC_LOAD).
    let flags = |position: u32| {
        sections
            .get(position as usize)
            .map_or(0, |s| gnu_flags(s.flags, s.sh_type, s.name))
    };
    let removed_flags = engine
        .outs
        .get(removed as usize)
        .map_or(0, |o| o.input_flags & !sec::LOAD);
    for same_region in [true, false] {
        let prev = statements
            .get(..at)?
            .iter()
            .rev()
            .find_map(|s| kept(s, same_region));
        let next = statements
            .get(at.saturating_add(1)..)?
            .iter()
            .find_map(|s| kept(s, same_region));
        let best = match (prev, next) {
            (None, next) => next,
            (Some(prev), None) => Some(prev),
            (Some(prev), Some(next)) => {
                let (pf, nf) = (flags(prev), flags(next));
                let use_prev = if (pf ^ nf) & (sec::ALLOC | sec::THREAD_LOCAL | sec::LOAD) != 0 {
                    (nf ^ removed_flags) & (sec::ALLOC | sec::THREAD_LOCAL) != 0
                        || (pf & sec::LOAD != 0 && nf & sec::LOAD == 0)
                } else if (pf ^ nf) & sec::READONLY != 0 {
                    (nf ^ removed_flags) & sec::READONLY != 0
                } else if (pf ^ nf) & sec::CODE != 0 {
                    (nf ^ removed_flags) & sec::CODE != 0
                } else {
                    sections
                        .get(next as usize)
                        .is_some_and(|s| address < s.addr)
                };
                Some(if use_prev { prev } else { next })
            }
        };
        if best.is_some() {
            return best;
        }
    }
    None
}

/// How the members matched by one description are ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SortRule {
    mode: SortMode,
    reverse: bool,
    /// `SORT(file)`: by file name first.
    pub(super) files: bool,
}

/// The sort rule of input description `sub` of `stmt`, with
/// `--sort-section` applied to descriptions that do not sort themselves;
/// `None` keeps input order.
pub(super) fn sort_rule(stmt: &OutputStmt, sub: u16, sort_section: SortMode) -> Option<SortRule> {
    let description = stmt.items.iter().find_map(|item| match item {
        Item::Input { description, index } if *index == sub => Some(description),
        _ => None,
    })?;
    let specs = description.sections.as_deref().unwrap_or_default();
    let mode = specs.first().map_or(SortMode::None, |s| s.sort);
    let reverse = specs.first().is_some_and(|s| s.reverse);
    if specs.iter().any(|s| s.sort != mode || s.reverse != reverse) {
        return None;
    }
    let mode = if mode == SortMode::None {
        sort_section
    } else {
        mode
    };
    let files = description.file.sort == SortMode::Name;
    (mode != SortMode::None || files).then_some(SortRule {
        mode,
        reverse,
        files,
    })
}

/// The `--sort-section` mode.
pub(super) fn sort_section_mode(options: &crate::args::LinkOptions) -> SortMode {
    match options.sort_section {
        crate::args::SortSection::Name => SortMode::Name,
        crate::args::SortSection::Alignment => SortMode::Alignment,
        _ => SortMode::None,
    }
}

/// Information for sorting one input member.
pub(super) struct SortInfo<'n> {
    /// 1 for input sections; linker-generated ones sort around them.
    pub(super) class: u8,
    /// The file (archive) path.
    pub(super) file: &'n [u8],
    /// The archive member name, or empty.
    pub(super) member: &'n [u8],
    /// The section name.
    pub(super) name: &'n [u8],
    /// The alignment.
    pub(super) align: u64,
    /// The section ID, for input order.
    pub(super) id: u32,
}

/// GNU's `compare_section`.
pub(super) fn compare_sections(
    rule: SortRule,
    a: &SortInfo<'_>,
    b: &SortInfo<'_>,
) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let by_name = || {
        if rule.reverse {
            b.name.cmp(a.name)
        } else {
            a.name.cmp(b.name)
        }
    };
    let by_align = || {
        if rule.reverse {
            a.align.cmp(&b.align)
        } else {
            b.align.cmp(&a.align)
        }
    };
    match rule.mode {
        SortMode::None | SortMode::NoSort => Ordering::Equal,
        SortMode::Name => by_name(),
        SortMode::Alignment => by_align(),
        SortMode::NameAlignment => by_name().then_with(by_align),
        SortMode::AlignmentName => by_align().then_with(by_name),
        SortMode::InitPriority => {
            let pa = crate::script::init_priority(a.name);
            let pb = crate::script::init_priority(b.name);
            match (pa, pb) {
                (Some(pa), Some(pb)) => {
                    let order = if rule.reverse {
                        pb.cmp(&pa)
                    } else {
                        pa.cmp(&pb)
                    };
                    order.then_with(by_name)
                }
                _ => by_name(),
            }
        }
    }
}

/// The members of every output section, in order.
#[allow(clippy::too_many_lines)]
fn build_entries(
    input: &LayoutInput<'_, '_>,
    script: &LayoutScript,
    placed: &ScriptPlacement,
) -> Result<Vec<Vec<Entry>>> {
    let placement = input.placement;
    let sections = input.sections;
    let files = input.files;
    let count = placement.outputs.len();
    let mut lists: Vec<Vec<(Entry, SortInfo<'_>)>> = (0..count).map(|_| Vec::new()).collect();
    let sort_section = sort_section_mode(input.options);
    // Sort rules by (output, sub), computed on first use.
    let mut rules: HashMap<(u32, u16), Option<SortRule>, foldhash::fast::FixedState> =
        HashMap::with_hasher(hasher());
    let mut first_eh_output: Option<u32> = None;
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        let (file_name, member_name) = match file.file {
            Some(f) => match f.member() {
                Some(m) => (f.path().as_os_str().as_encoded_bytes(), m.as_bytes()),
                None => (f.path().as_os_str().as_encoded_bytes(), &b""[..]),
            },
            None => (&b""[..], &b""[..]),
        };
        for (index, section) in object.sections.iter().enumerate() {
            let Some(id) = sections.id(file_index, u32::try_from(index).unwrap_or(NONE)) else {
                continue;
            };
            if !sections.is_live(id) {
                continue;
            }
            let Some(output) = placement.output_of(id) else {
                continue;
            };
            let member = if section.kind == SectionKind::Merge {
                match input.merged.group_of(id) {
                    Some(group) if input.merged.group_first.get(group as usize) == Some(&id) => {
                        Member::Merge(group)
                    }
                    Some(_) => continue,
                    None => Member::Input(id),
                }
            } else {
                Member::Input(id)
            };
            if section.kind == SectionKind::EhFrame && first_eh_output.is_none() {
                first_eh_output = Some(output);
            }
            let sub = placement.sub.get(id.index()).copied().unwrap_or(0);
            let (size, align) = member_size(input, member)?;
            rules
                .entry((output, sub))
                .or_insert_with(|| sort_rule(placed.stmt(script, output)?, sub, sort_section));
            if let Some(list) = lists.get_mut(output as usize) {
                list.push((
                    Entry {
                        member,
                        sub,
                        size,
                        align: align.max(1),
                        offset: 0,
                    },
                    SortInfo {
                        class: 1,
                        file: file_name,
                        member: member_name,
                        name: section.name,
                        align: align.max(1),
                        id: id.as_u32(),
                    },
                ));
            }
        }
    }
    // Linker-generated sections belong to the first input object, after its
    // own sections, as GNU ld creates them there.
    let synthetic_id = files
        .iter()
        .enumerate()
        .find_map(|(index, file)| {
            file.object.as_ref()?;
            let base = *sections.base.get(index)?;
            let count = *sections.count.get(index)?;
            (base != NONE).then(|| base.saturating_add(count).saturating_sub(1))
        })
        .unwrap_or(0);
    for place in &placed.synthetic {
        if place.output == NONE {
            continue;
        }
        let (size, align) = input.synth.size_align(place.kind);
        // Raw formats link through BFD's generic linker, which makes no ELF
        // notes.
        let raw = input
            .options
            .output_format
            .as_ref()
            .is_some_and(crate::args::OutputFormat::is_raw);
        if size == 0 || (raw && matches!(place.kind, Synthetic::GnuProperty | Synthetic::BuildId)) {
            continue;
        }
        let class = if place.kind == Synthetic::Common {
            2
        } else {
            0
        };
        if let Some(list) = lists.get_mut(place.output as usize) {
            list.push((
                Entry {
                    member: Member::Synthetic(place.kind),
                    sub: place.sub,
                    size,
                    align: align.max(1),
                    offset: 0,
                },
                SortInfo {
                    class,
                    file: b"",
                    member: b"",
                    name: b"",
                    align: align.max(1),
                    id: if class == 2 { u32::MAX } else { synthetic_id },
                },
            ));
        }
    }
    if let Some(output) = first_eh_output {
        let (size, align) = input.synth.size_align(Synthetic::EhFrameEnd);
        if size > 0
            && let Some(list) = lists.get_mut(output as usize)
        {
            list.push((
                Entry {
                    member: Member::Synthetic(Synthetic::EhFrameEnd),
                    sub: u16::MAX,
                    size,
                    align: align.max(1),
                    offset: 0,
                },
                SortInfo {
                    class: 3,
                    file: b"",
                    member: b"",
                    name: b"",
                    align: 1,
                    id: u32::MAX,
                },
            ));
        }
    }
    let mut result = Vec::with_capacity(count);
    for (output, mut list) in lists.into_iter().enumerate() {
        let output = u32::try_from(output).unwrap_or(NONE);
        // `--symbol-ordering-file`: ordered sections first in their
        // description (`crate::elf::ordering`).
        let order = input.order.filter(|_| {
            placement
                .outputs
                .get(output as usize)
                .is_some_and(|o| crate::elf::ordering::reorders(o.name))
        });
        let ordered = |s: &SortInfo<'_>| (s.class == 1).then(|| SectionId::from_u32(s.id));
        list.sort_by(|(ea, sa), (eb, sb)| {
            ea.sub
                .cmp(&eb.sub)
                .then((sa.class >= 2).cmp(&(sb.class >= 2)))
                .then_with(|| {
                    order.map_or(core::cmp::Ordering::Equal, |o| {
                        o.compare(ordered(sa), ordered(sb))
                    })
                })
                .then_with(|| {
                    let rule = rules.get(&(output, ea.sub)).copied().flatten();
                    if sa.class != 1 || sb.class != 1 {
                        // Sorted descriptions put generated sections first,
                        // which keeps the order total.
                        return if rule.is_some() {
                            sa.class.cmp(&sb.class)
                        } else {
                            core::cmp::Ordering::Equal
                        };
                    }
                    match rule {
                        Some(rule) => {
                            let files = if rule.files {
                                sa.file.cmp(sb.file).then(sa.member.cmp(sb.member))
                            } else {
                                core::cmp::Ordering::Equal
                            };
                            files.then_with(|| compare_sections(rule, sa, sb))
                        }
                        None => core::cmp::Ordering::Equal,
                    }
                })
                .then(sa.id.cmp(&sb.id))
                .then(sa.class.cmp(&sb.class))
        });
        result.push(list.into_iter().map(|(e, _)| e).collect());
    }
    Ok(result)
}

/// The steps of every output section.
fn build_programs<'s>(
    script: &'s LayoutScript,
    placed: &'s ScriptPlacement,
    entries: &[Vec<Entry>],
) -> Vec<Vec<Step<'s>>> {
    let mut programs = Vec::with_capacity(entries.len());
    for (output, list) in entries.iter().enumerate() {
        let mut steps = Vec::new();
        let range = |sub_from: u16, sub_to: u32| {
            let start = list.partition_point(|e| e.sub < sub_from);
            let end = list.partition_point(|e| u32::from(e.sub) < sub_to);
            (start, end.max(start))
        };
        let Some(stmt) = placed.stmt(script, u32::try_from(output).unwrap_or(NONE)) else {
            programs.push(steps);
            continue;
        };
        let mut inputs = 0u32;
        for item in &stmt.items {
            match item {
                Item::Input { index, .. } => {
                    let (start, end) = range(*index, u32::from(*index).saturating_add(1));
                    steps.push(Step::Run(start, end));
                    inputs = inputs.max(u32::from(*index).saturating_add(1));
                }
                other => steps.push(Step::Item(other)),
            }
        }
        let tail_from = u16::try_from(inputs).unwrap_or(u16::MAX);
        let (start, end) = range(tail_from, u32::MAX);
        if start < end {
            steps.push(Step::Run(start, end));
        }
        programs.push(steps);
    }
    programs
}

/// Emits `errors` and returns the error that fails the link.
fn fail(input: &LayoutInput<'_, '_>, script: &LayoutScript, errors: &[(Span, String)]) -> Error {
    let render = |span: &Span, message: &str| {
        if span.line == 0 {
            message.to_string()
        } else {
            format!(
                "{}:{}: {message}",
                script.file_of(*span).display(),
                span.line
            )
        }
    };
    match input.rules.diagnostics {
        Some(sink) => {
            for (span, message) in errors {
                sink.emit(Diagnostic::error(render(span, message)));
            }
            Error::Reported {
                errors: errors.len(),
            }
        }
        None => Error::Option(
            errors
                .iter()
                .map(|(span, message)| render(span, message))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    }
}

/// Runs layout under a linker script; see the [module documentation](self).
///
/// # Errors
///
/// Script evaluation errors, failed assertions, region overflows and
/// non-convergence are reported to the diagnostic sink and returned as
/// [`Error::Reported`].
#[allow(clippy::too_many_lines)]
pub fn layout<'a>(
    input: &LayoutInput<'_, 'a>,
    script: &LayoutScript,
    placed: &ScriptPlacement,
) -> Result<Layout<'a>> {
    let layout = layout_with(input, script, placed, None)?;
    // GNU ld lays out again when the program headers outgrow the space
    // SIZEOF_HEADERS estimated (`ldelf_map_segments`); the first layout
    // then does not fail for lack of room.
    let kind = input.kind();
    let needed = kind.ehdr_size().saturating_add(
        kind.phdr_size()
            .saturating_mul(u64::try_from(layout.segments.len()).unwrap_or(u64::MAX)),
    );
    if script.phdrs.is_none()
        && engine_used_sizeof_headers(script, placed)
        && layout.headers_reserved < needed
    {
        return layout_with(input, script, placed, Some(needed));
    }
    Ok(layout)
}

fn layout_with<'a>(
    input: &LayoutInput<'_, 'a>,
    script: &LayoutScript,
    placed: &ScriptPlacement,
    headers_override: Option<u64>,
) -> Result<Layout<'a>> {
    let placement = input.placement;
    let options = input.options;
    let entries = build_entries(input, script, placed)?;
    let programs = build_programs(script, placed, &entries);
    let count = placement.outputs.len();
    let resolved = placed.resolved.get();

    // Output state known before assignment.
    let mut outs: Vec<OutState> = vec![OutState::default(); count];
    let needed = |name: &[u8]| {
        placed
            .symbol_names
            .iter()
            .position(|n| n == name)
            .and_then(|slot| resolved.and_then(|r| r.needed.get(slot).copied()))
            .unwrap_or(true)
    };
    for (index, out) in outs.iter_mut().enumerate() {
        let index32 = u32::try_from(index).unwrap_or(NONE);
        let Some(stmt) = placed.stmt(script, index32) else {
            continue;
        };
        out.has_input = entries.get(index).is_some_and(|e| !e.is_empty());
        out.input_flags = placed.input_flags.get(index).copied().unwrap_or(0);
        if let Some(list) = entries.get(index) {
            for entry in list {
                if let Member::Synthetic(kind) = entry.member {
                    let (sh_flags, sh_type) = synthetic_flags(kind);
                    let f = gnu_flags(sh_flags | SHF_ALLOC, sh_type, b"");
                    out.input_flags |= f & !sec::READONLY;
                    if sh_flags & SHF_WRITE != 0 {
                        out.input_flags &= !sec::READONLY;
                    }
                }
            }
        }
        let mut other_items = false;
        for item in &stmt.items {
            match item {
                Item::Input { .. } => {}
                Item::Assign { assignment, .. } => {
                    other_items = true;
                    if assignment.is_dot() {
                        if !is_trivial_dot_expr(&assignment.expr) {
                            out.keep = true;
                        }
                    } else {
                        let provide = matches!(
                            assignment.kind,
                            AssignKind::Provide | AssignKind::ProvideHidden
                        );
                        if !provide || needed(&assignment.target) {
                            out.symbols_inside = true;
                        }
                    }
                }
                Item::Data { .. } | Item::Asciz(_) | Item::LinkerVersion => {
                    other_items = true;
                    out.has_data = true;
                }
                Item::Fill(_) | Item::Assert { .. } => other_items = true,
            }
        }
        if stmt.update_dot.is_some() {
            out.symbols_inside = true;
        }
        out.exists = !stmt.is_discard() && (out.has_input || other_items);
    }
    // Sections named by ADDR and LOADADDR exist.
    let mut by_name: HashMap<&[u8], u32, foldhash::fast::FixedState> =
        HashMap::with_hasher(hasher());
    for statement in &placed.statements {
        if let Statement::Output(i) = statement
            && placed.enabled.get(*i as usize).copied().unwrap_or(false)
            && let Some(stmt) = placed.stmt(script, *i)
            && !stmt.is_discard()
        {
            by_name.entry(stmt.name.as_slice()).or_insert(*i);
        }
    }
    let mut addressed: Vec<Vec<u8>> = Vec::new();
    for_each_expr(script, placed, &mut |expr| {
        if let Expr::Addr(name) | Expr::LoadAddr(name) = expr {
            addressed.push(name.clone());
        }
    });
    for name in &addressed {
        if let Some(&index) = by_name.get(name.as_slice())
            && let Some(out) = outs.get_mut(index as usize)
        {
            out.exists = true;
        }
    }

    let raw_output = options.output_format.as_ref().is_some_and(|f| {
        matches!(
            f.name(),
            "binary" | "ihex" | "srec" | "symbolsrec" | "verilog"
        )
    });
    let max_page = options
        .max_page_size
        .filter(|p| p.is_power_of_two())
        .unwrap_or(crate::elf::layout::DEFAULT_PAGE);
    let common_page = options
        .common_page_size
        .filter(|p| p.is_power_of_two())
        .unwrap_or(crate::elf::layout::DEFAULT_PAGE)
        .min(max_page);

    let relro_effective = options.relro && has_relro_section(input, script, placed, &entries);

    // SIZEOF_HEADERS: GNU's estimate of the program header count.
    let headers_size = if let Some(size) = headers_override {
        size
    } else if raw_output {
        0
    } else if let Some(phdrs) = &script.phdrs {
        input.kind().ehdr_size().saturating_add(
            input
                .kind()
                .phdr_size()
                .saturating_mul(u64::try_from(phdrs.len()).unwrap_or(0)),
        )
    } else {
        let mut segs: u64 = 2;
        let synth_exists = |kind: Synthetic| {
            input.synth.size_align(kind).0 > 0 && placed.synthetic_place(kind).is_some()
        };
        if synth_exists(Synthetic::Interp) {
            segs = segs.saturating_add(2);
        }
        if by_name.contains_key(&b".dynamic"[..]) && synth_exists(Synthetic::Dynamic) {
            segs = segs.saturating_add(1);
        }
        if relro_effective {
            segs = segs.saturating_add(1);
        }
        if input.synth.eh_frame_hdr && input.synth.fde_count > 0 {
            segs = segs.saturating_add(1);
        }
        let stack_note = input
            .files
            .iter()
            .filter_map(|f| f.object.as_ref())
            .any(|o| o.has_gnu_stack_note);
        if options.gnu_stack
            && (stack_note || options.exec_stack != crate::args::ExecStack::FromInputs)
        {
            segs = segs.saturating_add(1);
        }
        if synth_exists(Synthetic::GnuProperty) {
            segs = segs.saturating_add(1);
        }
        let mut previous_note: Option<u64> = None;
        let mut tls = false;
        for statement in &placed.statements {
            let Statement::Output(i) = statement else {
                continue;
            };
            let output = placement.outputs.get(*i as usize);
            let exists = outs
                .get(*i as usize)
                .is_some_and(|o| o.exists && o.has_input);
            let note =
                exists && output.is_some_and(|o| o.sh_type == SHT_NOTE && o.flags & SHF_ALLOC != 0);
            if note {
                let align = entries
                    .get(*i as usize)
                    .and_then(|e| e.iter().map(|x| x.align).max())
                    .unwrap_or(1);
                if previous_note != Some(align) {
                    segs = segs.saturating_add(1);
                }
                previous_note = Some(align);
            } else if exists {
                previous_note = None;
            }
            tls |= exists && output.is_some_and(|o| o.flags & SHF_TLS != 0);
        }
        if tls {
            segs = segs.saturating_add(1);
        }
        input
            .kind()
            .ehdr_size()
            .saturating_add(input.kind().phdr_size().saturating_mul(segs))
    };

    let mut regions: Vec<RegionState> = Vec::with_capacity(script.regions.len().saturating_add(1));
    let mut sym_index: HashMap<&[u8], usize, foldhash::fast::FixedState> =
        HashMap::with_hasher(hasher());
    for (slot, name) in placed.symbol_names.iter().enumerate() {
        sym_index.insert(name.as_slice(), slot);
    }
    let mut defs: HashMap<&[u8], SymbolDef, foldhash::fast::FixedState> =
        HashMap::with_hasher(hasher());
    if let Some(resolved) = resolved {
        for (name, def) in &resolved.defs {
            defs.insert(name.as_slice(), *def);
        }
    }
    let syms = placed
        .symbol_names
        .iter()
        .enumerate()
        .map(|(slot, _)| SymState {
            value: None,
            pass: 0,
            needed: resolved
                .and_then(|r| r.needed.get(slot).copied())
                .unwrap_or(true),
        })
        .collect();
    for region in &script.regions {
        regions.push(RegionState {
            attrs_flags: region.attributes.flags,
            attrs_not: region.attributes.not_flags,
            ..RegionState::default()
        });
    }
    let default_region = regions.len();
    regions.push(RegionState {
        length: u64::MAX,
        ..RegionState::default()
    });

    let mut engine = Engine {
        input,
        script,
        placed,
        entries,
        programs,
        outs,
        regions,
        default_region,
        syms,
        sym_index,
        defs,
        member_offset: vec![0; input.sections.len()],
        merge_offset: vec![0; input.merged.groups.len()],
        dot: 0,
        current: None,
        last_os: None,
        prefer_next: false,
        found_end: false,
        pass: 0,
        final_pass: false,
        errors: Vec::new(),
        warnings: Vec::new(),
        dataseg: DataSeg::default(),
        fill_patterns: Vec::new(),
        headers_size,
        headers_estimated: headers_override.is_none(),
        max_page,
        common_page,
        relro: relro_effective,
        span: Span::default(),
        by_name,
        lma_regions: Vec::new(),
    };
    engine.lma_regions = propagate_lma_regions(script, placed, count);
    // Region origins and lengths.
    let mut region_errors = Vec::new();
    for (index, region) in script.regions.iter().enumerate() {
        let origin = eval_absolute(&region.origin, &mut engine);
        let length = eval_absolute(&region.length, &mut engine);
        match (origin, length) {
            (Ok(origin), Ok(length)) => {
                if let Some(state) = engine.regions.get_mut(index) {
                    state.origin = origin;
                    state.length = length;
                }
            }
            (Err(e), _) | (_, Err(e)) => region_errors.push((region.span, e.to_string())),
        }
    }
    if !region_errors.is_empty() {
        return Err(fail(input, script, &region_errors));
    }

    // Passes until stable, then the DATA_SEGMENT adjustments, then until
    // stable again.
    let mut previous: Option<Snapshot> = None;
    let mut stable = false;
    for _ in 0..MAX_PASSES {
        engine.dataseg = DataSeg::default();
        engine.run_pass(false)?;
        let snapshot = engine.snapshot();
        if previous.as_ref() == Some(&snapshot) {
            stable = true;
            break;
        }
        previous = Some(snapshot);
    }
    let mut relro = None;
    if stable {
        if engine.dataseg.phase == SegPhase::EndSeen {
            let mut reset = false;
            if engine.relro && engine.dataseg.relro_end != 0 {
                let initial = engine.dataseg.base;
                let expected = relro_adjust(&mut engine);
                engine.run_pass(false)?;
                if engine.dataseg.relro_end > expected {
                    engine.dataseg.base = initial;
                    reset = true;
                }
            } else if size_segment(&mut engine.dataseg) {
                reset = true;
            }
            if reset {
                engine.run_pass(false)?;
            }
        } else {
            engine.dataseg.phase = SegPhase::Done;
        }
        stable = false;
        previous = None;
        for _ in 0..MAX_PASSES {
            let saved = engine.dataseg;
            engine.run_pass(false)?;
            engine.dataseg = saved;
            let snapshot = engine.snapshot();
            if previous.as_ref() == Some(&snapshot) {
                stable = true;
                break;
            }
            previous = Some(snapshot);
        }
        if engine.relro && engine.dataseg.relro_end != 0 {
            relro = Some((engine.dataseg.base, engine.dataseg.relro_end));
        }
    }
    let saved = engine.dataseg;
    engine.run_pass(true)?;
    engine.dataseg = saved;
    if !stable {
        engine.errors.push((
            Span::default(),
            format!("address assignment did not converge after {MAX_PASSES} passes"),
        ));
    }
    // Region overflow summaries.
    for (index, region) in engine.regions.iter().enumerate() {
        if region.full_message {
            let over = region
                .current
                .wrapping_sub(region.origin.wrapping_add(region.length));
            let name = script.regions.get(index).map_or_else(String::new, |r| {
                String::from_utf8_lossy(&r.name).into_owned()
            });
            let unit = if over == 1 { "byte" } else { "bytes" };
            engine.errors.push((
                Span::default(),
                format!("region `{name}' overflowed by {over} {unit}"),
            ));
        }
    }
    for (message, error) in &placed.reports {
        if *error {
            engine.errors.push((Span::default(), message.clone()));
        } else {
            engine.warnings.push(message.clone());
        }
    }
    if !engine.errors.is_empty() {
        let errors = std::mem::take(&mut engine.errors);
        return Err(fail(input, script, &errors));
    }
    assemble(engine, relro)
}

/// GNU's `lang_find_relro_sections`: whether a non-empty allocated input
/// section lies between the `DATA_SEGMENT_ALIGN` assignment and the
/// `DATA_SEGMENT_RELRO_END` one.
fn has_relro_section(
    input: &LayoutInput<'_, '_>,
    script: &LayoutScript,
    placed: &ScriptPlacement,
    entries: &[Vec<Entry>],
) -> bool {
    let contains = |statement: &Statement, want: fn(&Expr) -> bool| {
        let mut found = false;
        let mut check = |expr: &Expr| walk_expr(expr, &mut |e| found |= want(e));
        match statement {
            Statement::Assign { assignment, .. } => check(&assignment.expr),
            Statement::Assert { .. } => {}
            Statement::Output(index) => {
                if let Some(stmt) = placed.stmt(script, *index) {
                    for item in &stmt.items {
                        if let Item::Assign { assignment, .. } = item {
                            check(&assignment.expr);
                        }
                    }
                }
            }
        }
        found
    };
    let Some(start) = placed
        .statements
        .iter()
        .position(|s| contains(s, |e| matches!(e, Expr::DataSegmentAlign(..))))
    else {
        return false;
    };
    for statement in placed.statements.iter().skip(start) {
        if contains(statement, |e| matches!(e, Expr::DataSegmentRelroEnd(..))) {
            break;
        }
        let Statement::Output(index) = statement else {
            continue;
        };
        let Some(output) = input.placement.outputs.get(*index as usize) else {
            continue;
        };
        let tbss = output.flags & SHF_TLS != 0 && output.sh_type == SHT_NOBITS;
        let alloc = output.flags & SHF_ALLOC != 0;
        if tbss {
            continue;
        }
        if entries.get(*index as usize).is_some_and(|list| {
            list.iter()
                .any(|e| e.size > 0 && (alloc || matches!(e.member, Member::Synthetic(_))))
        }) {
            return true;
        }
    }
    false
}

/// GNU's `lang_propagate_lma_regions`: an output section with no load
/// region, load address or address, in the same region as the previous
/// output section statement, loads into that statement's load region.
fn propagate_lma_regions(
    script: &LayoutScript,
    placed: &ScriptPlacement,
    count: usize,
) -> Vec<Option<usize>> {
    let mut result = vec![None; count];
    let mut previous: Option<(Option<&[u8]>, Option<usize>)> = None;
    for statement in &placed.statements {
        let Statement::Output(index) = statement else {
            continue;
        };
        let Some(stmt) = placed.stmt(script, *index) else {
            continue;
        };
        let region = stmt.region.as_deref();
        let mut lma = stmt
            .load_region
            .as_deref()
            .and_then(|n| script.region_index(n));
        if lma.is_none()
            && stmt.load_address.is_none()
            && stmt.address.is_none()
            && let Some((previous_region, previous_lma)) = previous
            && previous_region == region
        {
            lma = previous_lma;
        }
        if let Some(slot) = result.get_mut(*index as usize) {
            *slot = lma;
        }
        previous = Some((region, lma));
    }
    result
}

/// GNU's `lang_size_relro_segment_1`: moves the start of the RELRO region
/// up so that it ends on a page boundary. Returns the expected end.
fn relro_adjust(engine: &mut Engine<'_, '_, '_>) -> u64 {
    let seg = engine.dataseg;
    let page = seg.max_page.max(1);
    let relro_end = align_up(seg.relro_end, page);
    let mut desired_end = relro_end.wrapping_sub(seg.relro_offset);
    let limit = seg.relro_end.wrapping_sub(seg.relro_offset);
    let emit_relocs = engine.emit_relocs();
    let order: Vec<u32> = engine
        .placed
        .statements
        .iter()
        .filter_map(|s| match s {
            Statement::Output(i) => Some(*i),
            _ => None,
        })
        .collect();
    for &index in order.iter().rev() {
        let Some(out) = engine.outs.get(index as usize) else {
            continue;
        };
        if !out.emitted(emit_relocs) || !engine.alloc_of(index) || !engine.enabled(index) {
            continue;
        }
        if out.vma >= seg.base && out.vma < limit {
            let mut end = out.vma;
            if !engine.is_tbss(index) {
                end = end.wrapping_add(out.size);
            }
            let bump = desired_end.wrapping_sub(end);
            let start = out.vma.wrapping_add(bump) & !out.align.max(1).wrapping_sub(1);
            desired_end = start;
        }
    }
    let seg = &mut engine.dataseg;
    seg.phase = SegPhase::RelroAdjust;
    if desired_end >= seg.base {
        seg.base = desired_end;
    }
    relro_end
}

/// GNU's `lang_size_segment`: whether a page can be saved at the start of
/// the data segment.
fn size_segment(seg: &mut DataSeg) -> bool {
    let page = seg.common_page.max(1);
    let first = seg.base.wrapping_neg() & page.wrapping_sub(1);
    let last = seg.end & page.wrapping_sub(1);
    if first != 0
        && last != 0
        && (seg.base & !page.wrapping_sub(1)) != (seg.end & !page.wrapping_sub(1))
        && first.wrapping_add(last) <= page
    {
        seg.phase = SegPhase::Adjust;
        return true;
    }
    seg.phase = SegPhase::Done;
    false
}

/// Turns the final assignment into a [`Layout`].
#[allow(clippy::too_many_lines)]
fn assemble<'a>(engine: Engine<'_, '_, 'a>, relro: Option<(u64, u64)>) -> Result<Layout<'a>> {
    let input = engine.input;
    let placement = input.placement;
    let script = engine.script;
    let placed = engine.placed;
    let options = input.options;
    let emit_relocs = options.emit_relocs;
    let count = placement.outputs.len();

    let mut out_sections: Vec<OutSection<'a>> = Vec::new();
    let mut output_places = vec![(0u64, 0u64, NONE); count];
    let mut section_phdrs: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut section_regions: Vec<Option<usize>> = Vec::new();
    let mut last_phdrs: Vec<Vec<u8>> = Vec::new();
    // Section header order is statement order, except that sections given
    // an address on the command line come first: GNU ld creates them
    // before reading the script.
    let mut order: Vec<u32> = Vec::new();
    for (name, _) in &options.section_starts {
        if let Some(&index) = engine.by_name.get(name.as_bytes())
            && !order.contains(&index)
        {
            order.push(index);
        }
    }
    let early = order.len();
    for statement in &placed.statements {
        if let Statement::Output(index) = statement
            && !order.get(..early).unwrap_or_default().contains(index)
        {
            order.push(*index);
        }
    }
    for index in order {
        let (Some(out), Some(output), Some(stmt)) = (
            engine.outs.get(index as usize),
            placement.outputs.get(index as usize),
            placed.stmt(script, index),
        ) else {
            continue;
        };
        if !engine.enabled(index) || !out.exists {
            continue;
        }
        if let Some(slot) = output_places.get_mut(index as usize) {
            *slot = (out.vma, out.vma.wrapping_add(out.size), NONE);
        }
        if !out.emitted(emit_relocs) {
            continue;
        }
        let list = engine
            .entries
            .get(index as usize)
            .map_or(&[][..], Vec::as_slice);
        let members: Vec<Placed> = list
            .iter()
            .map(|e| Placed {
                member: e.member,
                offset: e.offset,
                size: e.size,
            })
            .collect();
        let object_inputs = list
            .iter()
            .any(|e| !matches!(e.member, Member::Synthetic(_)));
        let mut flags = output.flags;
        let mut sh_type = output.sh_type;
        let mut first_synthetic = true;
        for e in list {
            if let Member::Synthetic(kind) = e.member {
                let (extra, synth_type) = synthetic_flags(kind);
                flags |= extra | SHF_ALLOC;
                if (!object_inputs && first_synthetic)
                    || (synth_type != SHT_NOBITS && sh_type == SHT_NOBITS)
                {
                    sh_type = synth_type;
                }
                first_synthetic = false;
            }
        }
        if out.dot_moved || out.has_data {
            flags |= SHF_ALLOC;
            if !object_inputs && !out.has_data {
                flags |= SHF_WRITE;
            }
        }
        if !object_inputs && first_synthetic {
            sh_type = if out.has_data {
                SHT_PROGBITS
            } else {
                SHT_NOBITS
            };
        } else if out.has_data && sh_type == SHT_NOBITS {
            sh_type = SHT_PROGBITS;
        }
        match &stmt.section_type {
            OutputSectionType::NoLoad => {
                if sh_type != SHT_NOTE {
                    sh_type = SHT_NOBITS;
                }
            }
            OutputSectionType::Info
            | OutputSectionType::Copy
            | OutputSectionType::DSect
            | OutputSectionType::Overlay => flags &= !SHF_ALLOC,
            OutputSectionType::ReadOnly => flags &= !SHF_WRITE,
            OutputSectionType::Type(expr) | OutputSectionType::ReadOnlyType(expr) => {
                if let Expr::Number(value) = expr {
                    sh_type = u32::try_from(*value).unwrap_or(sh_type);
                }
                if matches!(stmt.section_type, OutputSectionType::ReadOnlyType(_)) {
                    flags &= !SHF_WRITE;
                }
            }
            OutputSectionType::Normal => {}
        }
        if output.name == b".text" {
            // GNU ld forces `.text` read-only, except with -N.
            if options.magic == MagicMode::Omagic {
                flags |= SHF_WRITE;
            } else {
                flags &= !SHF_WRITE;
            }
        }
        let alloc = flags & SHF_ALLOC != 0;
        let position = u32::try_from(out_sections.len()).unwrap_or(NONE);
        if let Some(slot) = output_places.get_mut(index as usize) {
            slot.2 = position;
        }
        let entsize = entsize_of(input, index as usize, &members);
        let phdrs = if stmt.phdrs.is_empty() {
            if alloc && stmt.section_type != OutputSectionType::NoLoad {
                last_phdrs.clone()
            } else {
                Vec::new()
            }
        } else {
            last_phdrs = stmt.phdrs.clone();
            stmt.phdrs.clone()
        };
        section_phdrs.push(
            phdrs
                .into_iter()
                .filter(|n| n.as_slice() != b"NONE")
                .collect(),
        );
        section_regions.push(out.region.filter(|&r| r != engine.default_region));
        out_sections.push(OutSection {
            name: output.name,
            output: index,
            trailer: Trailer::None,
            sh_type,
            flags,
            addr: out.vma,
            offset: 0,
            size: out.size,
            align: out.align.max(1),
            entsize,
            link: 0,
            info: 0,
            members,
            name_offset: 0,
            name_prefix: b"",
            lma: out.lma,
            fills: out.fills.clone(),
            data: out.data.clone(),
        });
    }
    // Sections with no `:phdr` before the first one that has one take the
    // first list, as GNU ld does.
    if script.phdrs.is_some()
        && let Some(first) = section_phdrs.iter().find(|p| !p.is_empty()).cloned()
    {
        for (position, list) in section_phdrs.iter_mut().enumerate() {
            if !list.is_empty() {
                break;
            }
            if out_sections
                .get(position)
                .is_some_and(|s| s.flags & SHF_ALLOC != 0)
            {
                *list = first.clone();
            }
        }
    }

    // Compressed debug sections.
    let compressed: &[CompressedOutput] = input.compressed;
    for c in compressed {
        if let Some(section) = out_sections.iter_mut().find(|s| s.output == c.output) {
            section.size = c.size;
            if c.gnu {
                section.name_prefix = b".z";
                section.name = section.name.get(1..).unwrap_or(section.name);
                section.align = 1;
            } else {
                section.flags |= crate::elf::read::consts::SHF_COMPRESSED;
                section.align = 8;
            }
        }
    }
    let (section_symbols, shstrtab) = add_trailers(input, &mut out_sections)?;

    // Program headers.
    let phdr_specs: Option<Vec<super::segments::PhdrSpec>> = script.phdrs.as_ref().map(|list| {
        list.iter()
            .map(|p| {
                let mut ctx_value = |expr: &Expr| -> Option<u64> {
                    let mut probe = Probe;
                    eval_absolute(expr, &mut probe).ok()
                };
                super::segments::PhdrSpec {
                    name: p.name.clone(),
                    p_type: ctx_value(&p.phdr_type)
                        .and_then(|v| u32::try_from(v).ok())
                        .unwrap_or(0),
                    filehdr: p.filehdr,
                    phdrs: p.phdrs,
                    at: p.at.as_ref().and_then(&mut ctx_value),
                    flags: p
                        .flags
                        .as_ref()
                        .and_then(&mut ctx_value)
                        .and_then(|v| u32::try_from(v).ok()),
                }
            })
            .collect()
    });
    section_phdrs.resize(out_sections.len(), Vec::new());
    section_regions.resize(out_sections.len(), None);
    let position_of_synthetic = |kind: Synthetic| {
        out_sections.iter().position(|s| {
            s.members
                .iter()
                .any(|p| p.member == Member::Synthetic(kind))
        })
    };
    let segment_input = super::segments::SegmentInput {
        options,
        phdrs: phdr_specs.as_deref(),
        section_phdrs: &section_phdrs,
        regions: &section_regions,
        load_phdrs: engine_used_sizeof_headers(script, placed),
        kind: input.kind(),
        reserved_headers: engine.headers_size.max(input.kind().ehdr_size()),
        defer_room_error: engine.headers_estimated
            && phdr_specs.is_none()
            && engine_used_sizeof_headers(script, placed),
        relro,
        exec_stack: input.exec_stack,
        stack_note: input
            .files
            .iter()
            .filter_map(|f| f.object.as_ref())
            .any(|o| o.has_gnu_stack_note),
        interp: position_of_synthetic(Synthetic::Interp),
        eh_frame_hdr: position_of_synthetic(Synthetic::EhFrameHdr),
    };
    let result = super::segments::assign(&segment_input, &mut out_sections)?;
    set_links(&mut out_sections, input.synth);
    let shstrtab_len = u64::try_from(shstrtab.len()).unwrap_or(u64::MAX);
    for section in &mut out_sections {
        if section.trailer == Trailer::Shstrtab {
            section.size = shstrtab_len;
        }
    }
    // Trailers were given offsets in section order by `assign`; recompute
    // the shstrtab's now that its size is known.
    let mut file_end = result.file_end;
    for section in &mut out_sections {
        if section.trailer != Trailer::None {
            section.offset = crate::elf::layout::align_up(file_end, section.align.max(1))?;
            file_end = section
                .offset
                .checked_add(section.size)
                .ok_or_else(|| Error::Limit("output larger than the address space".into()))?;
        }
    }
    let shnum = u64::try_from(out_sections.len().saturating_add(1)).unwrap_or(u64::MAX);
    let shoff = crate::elf::layout::align_up(file_end, 8)?;
    let file_size = shoff
        .checked_add(shnum.saturating_mul(input.kind().shdr_size()))
        .ok_or_else(|| Error::Limit("output larger than the address space".into()))?;

    // Per input section addresses.
    let sections = input.sections;
    let mut section_addr = vec![0u64; sections.len()];
    let mut section_shndx = vec![0u32; sections.len()];
    let mut merge_place = vec![(0u64, 0u32); input.merged.groups.len()];
    for (index, list) in engine.entries.iter().enumerate() {
        let Some(&(vma, _, position)) = output_places.get(index) else {
            continue;
        };
        let exists = engine.outs.get(index).is_some_and(|o| o.exists);
        if !exists {
            continue;
        }
        let shndx = if position == NONE {
            crate::elf::layout::EMPTY_SHNDX
        } else {
            position.saturating_add(1)
        };
        for entry in list {
            let address = vma.wrapping_add(entry.offset);
            match entry.member {
                Member::Input(id) => {
                    if let Some(slot) = section_addr.get_mut(id.index()) {
                        *slot = address;
                    }
                    if let Some(slot) = section_shndx.get_mut(id.index()) {
                        *slot = shndx;
                    }
                }
                Member::Merge(group) => {
                    if let Some(slot) = merge_place.get_mut(group as usize) {
                        *slot = (address, shndx);
                    }
                }
                Member::Synthetic(_) => {}
            }
        }
    }
    for (id_index, shndx) in section_shndx.iter_mut().enumerate() {
        if *shndx != 0 {
            continue;
        }
        let id = SectionId::new(id_index);
        if let Some(group) = input.merged.group_of(id)
            && let Some(&(_, group_shndx)) = merge_place.get(group as usize)
        {
            *shndx = group_shndx;
        }
    }
    let mut synthetic_places = Vec::new();
    for section in &out_sections {
        for p in &section.members {
            if let Member::Synthetic(kind) = p.member {
                synthetic_places.push((
                    kind,
                    section.addr.wrapping_add(p.offset),
                    section.offset.wrapping_add(p.offset),
                    p.size,
                ));
            }
        }
    }
    // TLS template.
    let mut tls: Option<crate::elf::layout::Tls> = None;
    for section in &out_sections {
        if section.flags & SHF_TLS == 0 || section.flags & SHF_ALLOC == 0 {
            continue;
        }
        let t = tls.get_or_insert(crate::elf::layout::Tls {
            start: section.addr,
            memsz: 0,
            align: 1,
        });
        t.memsz = section
            .addr
            .wrapping_add(section.size)
            .wrapping_sub(t.start);
        t.align = t.align.max(section.align);
    }
    let base = result
        .segments
        .iter()
        .filter(|s| s.p_type == crate::elf::read::consts::PT_LOAD)
        .map(|s| s.vaddr)
        .min()
        .unwrap_or(0);
    // Script symbol values.
    let script_symbols = placed
        .symbol_names
        .iter()
        .zip(&engine.syms)
        .map(|(name, sym)| match sym.value {
            Some(value) => ScriptSymbol {
                name: name.clone(),
                value: value.resolve(&engine),
                absolute: !matches!(value.section, ValueSection::Relative(_)) && !value.from_dot,
                defined: true,
                section: match value.section {
                    ValueSection::Relative(output) => output_places
                        .get(output as usize)
                        .map(|p| p.2)
                        .filter(|&p| p != NONE)
                        .or_else(|| {
                            nearby_section(
                                &engine,
                                &output_places,
                                &out_sections,
                                output,
                                value.resolve(&engine),
                            )
                        }),
                    _ => None,
                },
            },
            None => ScriptSymbol {
                name: name.clone(),
                ..ScriptSymbol::default()
            },
        })
        .collect();
    let warnings = engine
        .warnings
        .iter()
        .map(|w| Diagnostic::warning(w.clone()))
        .collect();
    Ok(Layout {
        kind: input.kind(),
        sections: out_sections,
        // Script-driven layout does not insert range-extension thunks.
        thunks: Vec::new(),
        relax: Default::default(),
        output_places,
        section_addr,
        section_shndx,
        merge_place,
        synthetic: synthetic_places,
        segments: result.segments,
        tls,
        base,
        shoff,
        file_size,
        shstrtab,
        etext: 0,
        edata: 0,
        bss_start: 0,
        end: 0,
        section_symbols,
        fill_patterns: engine.fill_patterns,
        script_symbols,
        warnings,
        phoff: result.phoff,
        headers_reserved: engine.headers_size,
        nocrossrefs: script
            .nocrossrefs
            .iter()
            .map(|list| (list.to, list.sections.clone()))
            .collect(),
    })
}

/// Whether any expression uses `SIZEOF_HEADERS`.
fn engine_used_sizeof_headers(script: &LayoutScript, placed: &ScriptPlacement) -> bool {
    let mut used = false;
    for_each_expr(script, placed, &mut |expr| {
        used |= matches!(expr, Expr::SizeOfHeaders);
    });
    used
}

/// A context for constant expressions (`PHDRS` fields).
struct Probe;

impl EvalContext for Probe {
    type Section = u32;

    fn section_vma(&self, _section: u32) -> u64 {
        0
    }
}
