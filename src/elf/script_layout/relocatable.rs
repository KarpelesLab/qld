//! Linker scripts in relocatable links (`-r`).
//!
//! A relocatable link has no addresses, but GNU ld still runs a script: the
//! one `-T` names (the Linux kernel links every module with
//! `-r -T scripts/module.lds`), or else its built-in relocatable layout
//! ([`super::defaults::relocatable_script`], used on x86-64). Either way,
//! output section statements collect the input sections their patterns
//! match, in description order and with `SORT_*` applied; `/DISCARD/`
//! removes sections; data commands, `FILL` and assignments to `.` add
//! bytes; and symbol assignments define symbols relative to their output
//! section (or absolute ones outside output sections).
//!
//! [`place`] matches input sections with the final-link matcher, which
//! knows what a relocatable link differs in: as in GNU ld's
//! `unique_section_p`, members of COMDAT groups only match `/DISCARD/`, and
//! they get an orphan statement each. The relocatable writer
//! ([`crate::elf::relocatable`]) groups orphans itself and writes them
//! after the script's sections in input order, as GNU ld does. [`layout`]
//! then lays out each statement's members and evaluates the script's
//! expressions, repeating until symbol values settle.
//!
//! Relocatable output keeps no addresses, but expressions see the ones GNU
//! ld computes while it runs the script, and `. = ALIGN(n)` pads by them:
//! a section without an address starts where the last allocated section
//! of the default memory region ended, and orphans (placed among the
//! statements as in final links) start at 0. Header addresses are written
//! as 0.

#![deny(clippy::arithmetic_side_effects)]

use hashbrown::HashMap;

use crate::args::LinkOptions;
use crate::elf::inputs::ElfInput;
use crate::elf::place::Placement;
use crate::elf::read::SectionIndex;
use crate::elf::read::consts::{SHF_ALLOC, SHF_WRITE};
use crate::elf::refs::LINKER_FILE;
use crate::elf::sections::{NONE, Sections};
use crate::error::{Error, Result};
use crate::ids::{SectionId, SymbolId};
use crate::script::{
    AssignKind, EvalContext, EvalError, Fill, OutputSectionType, Value, ValueSection, align_up,
    eval, eval_dot_assignment, eval_symbol_assignment, fill_pattern,
};
use crate::symbols::{
    Definition, DefinitionKind, InputPosition, SymbolFlags, SymbolName, SymbolTable,
};

use super::engine::{SortInfo, compare_sections, sort_rule, sort_section_mode};
use super::plan::{Item, LayoutScript, OutputStmt, Statement};

/// Most passes over the script before symbol values must have settled.
const MAX_PASSES: u32 = 8;

/// Where a script symbol of a relocatable link is defined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptValue {
    /// An absolute value.
    Absolute(u64),
    /// An offset into a script output section (index in
    /// [`RelocatableScript::outputs`]).
    Output(u32, u64),
    /// An offset from the start of an input section, wherever the writer
    /// puts it.
    Input(SectionId, u64),
}

/// A symbol a relocatable link's script defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScriptDefinition {
    /// The symbol.
    pub id: SymbolId,
    /// Its value.
    pub value: ScriptValue,
    /// `HIDDEN` or `PROVIDE_HIDDEN`.
    pub hidden: bool,
    /// The symbol a plain `sym = other;` copies its type from.
    pub type_from: Option<SymbolId>,
}

/// One member of a script output section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScriptMember {
    /// The input file.
    pub file: u32,
    /// The section index in that file.
    pub section: u32,
    /// Its offset in the output section.
    pub offset: u64,
}

/// An output section a script statement creates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptOutput<'a> {
    /// Its name.
    pub name: &'a [u8],
    /// Its input sections, in order.
    pub members: Vec<ScriptMember>,
    /// Bytes of data commands (`BYTE`, `LONG`, `ASCIZ`, ...) and filled
    /// gaps, by offset.
    pub data: Vec<(u64, Vec<u8>)>,
    /// Its size.
    pub size: u64,
    /// Its alignment: the largest of its members' and its `ALIGN`.
    pub align: u64,
    /// The address GNU ld gives it while running the script (relocatable
    /// output keeps none; link-order sections sort by it).
    pub vma: u64,
    /// The flags it has without input sections: `SHF_ALLOC` for data
    /// commands, `SHF_WRITE` for a section created by assignments alone
    /// (GNU ld's `init_os` with no flags).
    pub flags: u64,
    /// `NOLOAD`: `SHT_NOBITS`.
    pub nobits: bool,
    /// The fill pattern of gaps between members, when the script gives one.
    pub fill: Option<Vec<u8>>,
}

/// The script's part of a relocatable link.
#[derive(Clone, Debug, Default)]
pub struct RelocatableScript<'a> {
    /// Output sections created by script statements, in statement order.
    pub outputs: Vec<ScriptOutput<'a>>,
    /// The index in [`RelocatableScript::outputs`] of each input section
    /// the script places, by section ID ([`NONE`] for the others).
    pub assign: Vec<u32>,
    /// The symbols the script defines, by symbol ID.
    pub symbols: Vec<ScriptDefinition>,
    /// Whether a statement names `.note.gnu.build-id`, which then comes
    /// first (as in GNU ld's built-in `-r` layout).
    pub build_id_first: bool,
}

impl RelocatableScript<'_> {
    /// The definition of symbol `id`, if the script defines it.
    #[must_use]
    pub fn symbol(&self, id: SymbolId) -> Option<&ScriptDefinition> {
        self.symbols
            .binary_search_by_key(&id, |d| d.id)
            .ok()
            .and_then(|at| self.symbols.get(at))
    }
}

/// Matches the live input sections of a relocatable link against the
/// script, and marks the ones `/DISCARD/` removes dead. Orphans (group
/// members among them) get statements of their own, placed as GNU ld
/// places them; only their position matters here (they start at address 0
/// and move the next section's start), as the relocatable writer groups
/// and orders them itself.
pub fn place<'a, F: crate::elf::read::ElfFormat>(
    script: &'a LayoutScript,
    files: &[ElfInput<'a, F>],
    sections: &mut Sections,
    options: &LinkOptions,
) -> Placement<'a> {
    let placement = super::place(script, files, sections, options);
    for id in &placement.discarded {
        if let Some(live) = sections.live.get_mut(id.index()) {
            *live = false;
        }
    }
    placement
}

/// A section handle in expressions: a script output or an input section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handle {
    Output(u32),
    Input(SectionId),
}

/// Expression state for one pass.
struct Context<'c, 'a, F: crate::elf::read::ElfFormat = crate::elf::read::Elf64Le> {
    script: &'c LayoutScript,
    files: &'c [ElfInput<'a, F>],
    sections: &'c Sections,
    symbols: &'c SymbolTable<'a>,
    options: &'c LinkOptions,
    /// Script output index of each statement (by statement output index).
    output_of_stmt: &'c [u32],
    /// Script output of each input section and its offset, from the
    /// previous pass (or this one, once laid out).
    placed: &'c HashMap<SectionId, (u32, u64), foldhash::fast::FixedState>,
    /// Sizes and alignments of the script outputs.
    sizes: &'c [u64],
    aligns: &'c [u64],
    /// The addresses GNU ld gives the script outputs: this pass's for the
    /// outputs laid out so far, the previous pass's for the others.
    vmas: Vec<u64>,
    /// Values assigned so far (this pass, then the previous one).
    values: HashMap<Vec<u8>, Value<Handle>, foldhash::fast::FixedState>,
    previous: &'c HashMap<Vec<u8>, Value<Handle>, foldhash::fast::FixedState>,
    current: Option<u32>,
    dot: u64,
    errors: Vec<String>,
}

impl<F: crate::elf::read::ElfFormat> Context<'_, '_, F> {
    fn output_named(&self, name: &[u8]) -> Option<u32> {
        self.script.statements.iter().find_map(|s| match s {
            Statement::Output(i) => {
                let out = self.output_of_stmt.get(*i as usize).copied()?;
                (out != NONE && self.script.output(*i).is_some_and(|o| o.name == name))
                    .then_some(out)
            }
            _ => None,
        })
    }

    /// The value of a symbol an input file defines.
    fn input_symbol(&self, id: SymbolId) -> Option<Value<Handle>> {
        let def = self.symbols.definition(id);
        if !matches!(def.kind, DefinitionKind::Regular | DefinitionKind::Weak)
            || def.file == LINKER_FILE
        {
            return None;
        }
        let file = def.file.index();
        let object = self.files.get(file)?.object.as_ref()?;
        let table = object.elf.symbols();
        let index = (def.index as usize).checked_add(object.first_global)?;
        let raw = table.get_raw(index)?;
        match table.section(index, &raw).ok()? {
            SectionIndex::Section(section) => {
                let id = self.sections.id(file, section)?;
                Some(match self.placed.get(&id) {
                    Some(&(out, offset)) => {
                        Value::relative(Handle::Output(out), offset.wrapping_add(raw.st_value))
                    }
                    None => Value::relative(Handle::Input(id), raw.st_value),
                })
            }
            SectionIndex::Absolute => Some(Value::absolute(raw.st_value)),
            _ => None,
        }
    }
}

impl<F: crate::elf::read::ElfFormat> EvalContext for Context<'_, '_, F> {
    type Section = Handle;

    fn section_vma(&self, section: Handle) -> u64 {
        match section {
            Handle::Output(out) => self.vmas.get(out as usize).copied().unwrap_or(0),
            Handle::Input(_) => 0,
        }
    }

    fn current_section(&self) -> Option<Handle> {
        self.current.map(Handle::Output)
    }

    fn dot(&self) -> std::result::Result<u64, EvalError> {
        Ok(self.dot)
    }

    fn sane_expr(&self) -> bool {
        self.script.sane_expr
    }

    fn symbol(&mut self, name: &[u8]) -> std::result::Result<Value<Handle>, EvalError> {
        if let Some(value) = self.values.get(name).or_else(|| self.previous.get(name)) {
            return Ok(*value);
        }
        self.symbols
            .lookup(&SymbolName::new(name))
            .and_then(|id| self.input_symbol(id))
            .ok_or_else(|| EvalError::UndefinedSymbol(name.to_vec()))
    }

    fn is_defined(&mut self, name: &[u8]) -> bool {
        self.values.contains_key(name)
            || self
                .symbols
                .lookup(&SymbolName::new(name))
                .is_some_and(|id| {
                    matches!(
                        self.symbols.definition_kind(id),
                        DefinitionKind::Regular | DefinitionKind::Weak | DefinitionKind::Common
                    ) && self.symbols.definition(id).file != LINKER_FILE
                })
    }

    fn section_addr(&mut self, name: &[u8]) -> std::result::Result<Value<Handle>, EvalError> {
        self.output_named(name)
            .map(|out| Value::relative(Handle::Output(out), 0))
            .ok_or_else(|| EvalError::UndefinedSection(name.to_vec()))
    }

    fn section_load_addr(&mut self, name: &[u8]) -> std::result::Result<u64, EvalError> {
        self.output_named(name)
            .map(|out| self.section_vma(Handle::Output(out)))
            .ok_or_else(|| EvalError::UndefinedSection(name.to_vec()))
    }

    fn section_size(&mut self, name: &[u8]) -> std::result::Result<u64, EvalError> {
        Ok(self
            .output_named(name)
            .and_then(|out| self.sizes.get(out as usize).copied())
            .unwrap_or(0))
    }

    fn section_alignment(&mut self, name: &[u8]) -> std::result::Result<u64, EvalError> {
        self.output_named(name)
            .and_then(|out| self.aligns.get(out as usize).copied())
            .ok_or_else(|| EvalError::UndefinedSection(name.to_vec()))
    }

    fn sizeof_headers(&mut self) -> std::result::Result<u64, EvalError> {
        Ok(64)
    }

    fn max_page_size(&self) -> std::result::Result<u64, EvalError> {
        Ok(self.options.max_page_size.unwrap_or(0x1000))
    }

    fn common_page_size(&self) -> std::result::Result<u64, EvalError> {
        Ok(self.options.common_page_size.unwrap_or(0x1000))
    }

    fn assertion_failed(&mut self, message: &[u8]) -> std::result::Result<(), EvalError> {
        Err(EvalError::AssertionFailed(message.to_vec()))
    }
}

/// Which script symbols a relocatable link defines: every plain
/// assignment, and a `PROVIDE` only when an input refers to the symbol and
/// none defines it (or another script expression reads it).
fn applied_symbols(
    symbols: &SymbolTable<'_>,
    script: &LayoutScript,
    placement: &Placement<'_>,
) -> Vec<(Vec<u8>, SymbolId, bool, bool)> {
    let Some(placed) = placement.script.as_deref() else {
        return Vec::new();
    };
    let mut applied = Vec::new();
    for (slot, name) in placed.symbol_names.iter().enumerate() {
        let (provide, hidden) = placed
            .symbol_kinds
            .get(slot)
            .copied()
            .unwrap_or((false, false));
        let Some(id) = symbols.lookup(&SymbolName::new(name)) else {
            continue;
        };
        let apply = !provide
            || (matches!(
                symbols.definition_kind(id),
                DefinitionKind::Undefined | DefinitionKind::Lazy
            ) && (symbols
                .flags(id)
                .intersects(SymbolFlags::REFERENCED | SymbolFlags::WEAK_REFERENCED)
                || script.referenced.iter().any(|(n, _)| n == name)));
        if apply {
            applied.push((name.clone(), id, provide, hidden));
        }
    }
    applied
}

/// Lays out a relocatable link's script outputs and evaluates its symbols;
/// see the [module documentation](self). The symbols it defines become
/// definitions of the linker.
///
/// # Errors
///
/// Undefined symbols in expressions, failed `ASSERT`s, and values that do
/// not settle.
#[allow(clippy::too_many_lines)]
pub fn layout<'a, F: crate::elf::read::ElfFormat>(
    script: &'a LayoutScript,
    placement: &Placement<'a>,
    files: &[ElfInput<'a, F>],
    sections: &Sections,
    symbols: &SymbolTable<'a>,
    options: &LinkOptions,
) -> Result<RelocatableScript<'a>> {
    let placed = placement.script.as_deref();
    let enabled =
        |stmt: u32| placed.is_none_or(|p| p.enabled.get(stmt as usize).copied().unwrap_or(false));

    // Members of each statement by description, in input order.
    let mut by_stmt: Vec<Vec<(u16, SectionId)>> = vec![Vec::new(); script.outputs.len()];
    let mut assign_stmt = vec![NONE; sections.len()];
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for index in 0..object.sections.len() {
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            let Some(id) = sections.id(file_index, index) else {
                continue;
            };
            if !sections.is_live(id) {
                continue;
            }
            let Some(&stmt) = placement.out.get(id.index()) else {
                continue;
            };
            if stmt == NONE {
                continue;
            }
            let sub = placement.sub.get(id.index()).copied().unwrap_or(0);
            if let Some(list) = by_stmt.get_mut(stmt as usize) {
                list.push((sub, id));
            }
            if let Some(slot) = assign_stmt.get_mut(id.index()) {
                *slot = stmt;
            }
        }
    }
    let sort_section = sort_section_mode(options);
    let info = |id: SectionId| -> Option<SortInfo<'a>> {
        let (file_index, index) = sections.locate(id)?;
        let file = files.get(file_index)?;
        let section = file.object.as_ref()?.section(index)?;
        let (path, member) = match file.file {
            Some(f) => (
                f.path().as_os_str().as_encoded_bytes(),
                f.member().map_or(&b""[..], str::as_bytes),
            ),
            None => (&b""[..], &b""[..]),
        };
        Some(SortInfo {
            class: 1,
            file: path,
            member,
            name: section.name,
            align: section.header.sh_addralign.max(1),
            id: id.as_u32(),
        })
    };
    for (stmt, list) in by_stmt.iter_mut().enumerate() {
        let Some(output) = script.outputs.get(stmt) else {
            continue;
        };
        list.sort_by(|&(sa, a), &(sb, b)| {
            sa.cmp(&sb).then_with(|| {
                let rule = sort_rule(output, sa, sort_section);
                match (rule, info(a), info(b)) {
                    (Some(rule), Some(ia), Some(ib)) => {
                        let files = if rule.files {
                            ia.file.cmp(ib.file).then(ia.member.cmp(ib.member))
                        } else {
                            core::cmp::Ordering::Equal
                        };
                        files
                            .then_with(|| compare_sections(rule, &ia, &ib))
                            .then(a.cmp(&b))
                    }
                    _ => a.cmp(&b),
                }
            })
        });
    }

    // Which statements create an output section: GNU ld's `init_os` runs
    // for statements with input sections, data commands, `FILL` or
    // assignments.
    let mut output_of_stmt = vec![NONE; script.outputs.len()];
    let mut outputs: Vec<ScriptOutput<'a>> = Vec::new();
    for statement in &script.statements {
        let Statement::Output(stmt) = statement else {
            continue;
        };
        let Some(output) = script.output(*stmt) else {
            continue;
        };
        if output.is_discard() || !enabled(*stmt) {
            continue;
        }
        let has_members = by_stmt.get(*stmt as usize).is_some_and(|l| !l.is_empty());
        let has_data = output
            .items
            .iter()
            .any(|i| matches!(i, Item::Data { .. } | Item::Asciz(_) | Item::LinkerVersion));
        let has_other = output
            .items
            .iter()
            .any(|i| matches!(i, Item::Assign { .. } | Item::Fill(_)));
        if !has_members && !has_data && !has_other {
            continue;
        }
        let Some(slot) = output_of_stmt.get_mut(*stmt as usize) else {
            continue;
        };
        if *slot != NONE {
            continue;
        }
        *slot = u32::try_from(outputs.len())
            .map_err(|_| Error::Limit("too many output sections".into()))?;
        outputs.push(ScriptOutput {
            name: &output.name,
            flags: if has_data {
                SHF_ALLOC
            } else if has_members {
                0
            } else {
                SHF_WRITE
            },
            nobits: output.section_type == OutputSectionType::NoLoad,
            align: 1,
            ..ScriptOutput::default()
        });
    }

    // Orphan statements: size (from address 0), allocation and whether
    // they exist, which is all their effect on the next statement's
    // address. `.note.gnu.property` is the merged note.
    let orphan_count = placed.map_or(0, |p| p.orphans.len());
    let mut orphans: Vec<(u64, bool, bool)> = vec![(0, false, false); orphan_count];
    let first_orphan = script.outputs.len();
    for (file_index, file) in files.iter().enumerate() {
        let Some(object) = &file.object else {
            continue;
        };
        for (index, section) in object.sections.iter().enumerate() {
            let Some(id) = u32::try_from(index)
                .ok()
                .and_then(|i| sections.id(file_index, i))
            else {
                continue;
            };
            if !sections.is_live(id) {
                continue;
            }
            let stmt = placement.out.get(id.index()).copied().unwrap_or(NONE);
            let Some(slot) = (stmt as usize)
                .checked_sub(first_orphan)
                .and_then(|o| orphans.get_mut(o))
            else {
                continue;
            };
            let header = &section.header;
            slot.0 = align_up(slot.0, header.sh_addralign).wrapping_add(header.sh_size);
            slot.1 |= header.sh_flags & SHF_ALLOC != 0;
            slot.2 = true;
        }
    }
    if let Some(note) = placed
        .and_then(|p| p.synthetic_place(crate::elf::rules::Synthetic::GnuProperty))
        .and_then(|p| (p.output as usize).checked_sub(first_orphan))
        .and_then(|o| orphans.get_mut(o))
        && let Some(bytes) = crate::elf::synth::plan_property_note(files, options)
    {
        *note = (
            note.0.wrapping_add(u64::try_from(bytes.len()).unwrap_or(0)),
            true,
            true,
        );
    }
    let alloc: Vec<bool> = outputs
        .iter()
        .zip(&output_of_stmt_list(&output_of_stmt, outputs.len()))
        .map(|(output, stmt)| {
            output.flags & SHF_ALLOC != 0
                || by_stmt.get(*stmt as usize).is_some_and(|list| {
                    list.iter().any(|&(_, id)| {
                        sections
                            .locate(id)
                            .and_then(|(f, i)| files.get(f)?.object.as_ref()?.section(i))
                            .is_some_and(|s| s.header.sh_flags & SHF_ALLOC != 0)
                    })
                })
        })
        .collect();
    let statements = placed.map_or(&script.statements, |p| &p.statements);

    // Symbols the script defines become the linker's.
    let applied = applied_symbols(symbols, script, placement);
    for (slot, (_, id, _, _)) in applied.iter().enumerate() {
        symbols.replace_definition(
            *id,
            &Definition {
                kind: DefinitionKind::Regular,
                file: LINKER_FILE,
                index: u32::try_from(slot).unwrap_or(u32::MAX),
                position: InputPosition::from_raw(u64::MAX),
                aux: 0,
            },
        );
    }
    let wanted = |name: &[u8]| applied.iter().any(|(n, ..)| n == name);

    let hasher = || foldhash::fast::FixedState::with_seed(0x7265_6c6f_6373);
    let mut previous: HashMap<Vec<u8>, Value<Handle>, _> = HashMap::with_hasher(hasher());
    let mut placed_at: HashMap<SectionId, (u32, u64), _> = HashMap::with_hasher(hasher());
    let mut sizes = vec![0u64; outputs.len()];
    let mut aligns = vec![1u64; outputs.len()];
    let mut vmas = vec![0u64; outputs.len()];
    let mut pass = 0u32;
    loop {
        pass = pass.saturating_add(1);
        let final_pass = pass >= MAX_PASSES;
        let mut new_placed: HashMap<SectionId, (u32, u64), _> = HashMap::with_hasher(hasher());
        let mut new_sizes = sizes.clone();
        let mut new_aligns = aligns.clone();
        let mut members: Vec<Vec<ScriptMember>> = vec![Vec::new(); outputs.len()];
        let mut data: Vec<Vec<(u64, Vec<u8>)>> = vec![Vec::new(); outputs.len()];
        let mut fills: Vec<Option<Vec<u8>>> = vec![None; outputs.len()];
        let mut ctx = Context {
            script,
            files,
            sections,
            symbols,
            options,
            output_of_stmt: &output_of_stmt,
            placed: &placed_at,
            sizes: &sizes,
            aligns: &aligns,
            vmas: vmas.clone(),
            values: HashMap::with_hasher(hasher()),
            previous: &previous,
            current: None,
            dot: 0,
            errors: Vec::new(),
        };
        // GNU ld's default memory region: where the next output section
        // without an address starts.
        let mut region = 0u64;
        for statement in statements {
            match statement {
                Statement::Assign { assignment, .. } => {
                    assign(&mut ctx, assignment, &wanted);
                    if assignment.is_dot() {
                        region = ctx.dot;
                    }
                }
                Statement::Assert { assert, .. } => {
                    if let Err(error) = eval(&assert.expr, &mut ctx).and_then(|v| {
                        if v.resolve(&ctx) == 0 {
                            ctx.assertion_failed(&assert.message)
                        } else {
                            Ok(())
                        }
                    }) {
                        ctx.errors.push(error.to_string());
                    }
                }
                Statement::Output(stmt) => {
                    if let Some(&(size, is_alloc, exists)) = (*stmt as usize)
                        .checked_sub(first_orphan)
                        .and_then(|o| orphans.get(o))
                    {
                        // Orphans start at 0 in relocatable links.
                        if exists {
                            ctx.dot = size;
                            if is_alloc {
                                region = size;
                            }
                        }
                        continue;
                    }
                    let Some(output) = script.output(*stmt) else {
                        continue;
                    };
                    if output.is_discard() || !enabled(*stmt) {
                        continue;
                    }
                    if let Some(expr) = &output.address {
                        ctx.current = None;
                        match eval(expr, &mut ctx) {
                            Ok(value) => ctx.dot = value.resolve(&ctx),
                            Err(error) => ctx.errors.push(error.to_string()),
                        }
                    }
                    let out = output_of_stmt.get(*stmt as usize).copied().unwrap_or(NONE);
                    if out == NONE {
                        continue;
                    }
                    let list = by_stmt.get(*stmt as usize).map_or(&[][..], Vec::as_slice);
                    let attr_align = match &output.align {
                        Some(expr) => match eval(expr, &mut ctx) {
                            Ok(value) => value.resolve(&ctx).max(1),
                            Err(error) => {
                                ctx.errors.push(error.to_string());
                                1
                            }
                        },
                        None => 1,
                    };
                    let subalign = match &output.subalign {
                        Some(expr) => match eval(expr, &mut ctx) {
                            Ok(value) => Some(value.resolve(&ctx).max(1)),
                            Err(error) => {
                                ctx.errors.push(error.to_string());
                                None
                            }
                        },
                        None => None,
                    };
                    let input_align = list
                        .iter()
                        .filter_map(|&(_, id)| {
                            let (f, i) = sections.locate(id)?;
                            let section = files.get(f)?.object.as_ref()?.section(i)?;
                            Some(subalign.unwrap_or(section.header.sh_addralign).max(1))
                        })
                        .max()
                        .unwrap_or(1);
                    let vma = if output.address.is_some() {
                        align_up(ctx.dot, attr_align)
                    } else {
                        align_up(region, attr_align.max(input_align))
                    };
                    if let Some(slot) = ctx.vmas.get_mut(out as usize) {
                        *slot = vma;
                    }
                    let result = lay_out_output(
                        &mut ctx,
                        output,
                        (out, vma, attr_align, subalign),
                        list,
                        files,
                        sections,
                        &wanted,
                    );
                    ctx.dot = vma.wrapping_add(result.size);
                    if alloc.get(out as usize).copied().unwrap_or(false) {
                        region = ctx.dot;
                    }
                    if let Some(slot) = new_sizes.get_mut(out as usize) {
                        *slot = result.size;
                    }
                    if let Some(slot) = new_aligns.get_mut(out as usize) {
                        *slot = result.align;
                    }
                    for member in &result.members {
                        if let Some(id) = sections.id(member.file as usize, member.section) {
                            new_placed.insert(id, (out, member.offset));
                        }
                    }
                    if let Some(slot) = members.get_mut(out as usize) {
                        *slot = result.members;
                    }
                    if let Some(slot) = data.get_mut(out as usize) {
                        *slot = result.data;
                    }
                    if let Some(slot) = fills.get_mut(out as usize) {
                        *slot = result.fill;
                    }
                }
            }
        }
        let values = std::mem::take(&mut ctx.values);
        let errors = std::mem::take(&mut ctx.errors);
        let new_vmas = std::mem::take(&mut ctx.vmas);
        let settled = values == previous
            && new_placed == placed_at
            && new_sizes == sizes
            && new_aligns == aligns
            && new_vmas == vmas;
        vmas = new_vmas;
        previous = values;
        placed_at = new_placed;
        sizes = new_sizes;
        aligns = new_aligns;
        if settled || final_pass {
            if let Some(error) = errors.first() {
                return Err(Error::Option(format!("{error} (in the linker script)")));
            }
            if !settled {
                return Err(Error::Option(
                    "linker script symbol values do not settle".into(),
                ));
            }
            for (index, output) in outputs.iter_mut().enumerate() {
                output.members = members
                    .get_mut(index)
                    .map(std::mem::take)
                    .unwrap_or_default();
                output.data = data.get_mut(index).map(std::mem::take).unwrap_or_default();
                output.fill = fills.get_mut(index).and_then(Option::take);
                output.size = sizes.get(index).copied().unwrap_or(0);
                output.vma = vmas.get(index).copied().unwrap_or(0);
                output.align = aligns.get(index).copied().unwrap_or(1);
            }
            break;
        }
    }

    let mut assign_out = vec![NONE; sections.len()];
    for (index, stmt) in assign_stmt.iter().enumerate() {
        if *stmt != NONE
            && let Some(slot) = assign_out.get_mut(index)
        {
            *slot = output_of_stmt.get(*stmt as usize).copied().unwrap_or(NONE);
        }
    }
    let mut definitions: Vec<ScriptDefinition> = applied
        .iter()
        .filter_map(|(name, id, _, hidden)| {
            let value = previous.get(name.as_slice())?;
            let value = match value.section {
                ValueSection::Relative(Handle::Output(out)) => {
                    ScriptValue::Output(out, value.value)
                }
                ValueSection::Relative(Handle::Input(section)) => {
                    ScriptValue::Input(section, value.value)
                }
                ValueSection::Absolute | ValueSection::Number => ScriptValue::Absolute(value.value),
            };
            Some(ScriptDefinition {
                id: *id,
                value,
                hidden: *hidden,
                type_from: copied_type(script, name, symbols),
            })
        })
        .collect();
    definitions.sort_by_key(|d| d.id);
    definitions.dedup_by_key(|d| d.id);
    Ok(RelocatableScript {
        outputs,
        assign: assign_out,
        symbols: definitions,
        build_id_first: script.find_output(b".note.gnu.build-id").is_some(),
    })
}

/// The symbol whose type a plain `name = other;` (the last assignment of
/// `name`) copies, as GNU ld's `try_copy_symbol_type` does.
fn copied_type(script: &LayoutScript, name: &[u8], symbols: &SymbolTable<'_>) -> Option<SymbolId> {
    let mut last = None;
    let mut visit = |assignment: &crate::script::Assignment| {
        if assignment.target == name {
            last = Some(
                assignment
                    .op
                    .binary()
                    .is_none()
                    .then(|| assignment.expr.type_source().map(<[u8]>::to_vec))
                    .flatten(),
            );
        }
    };
    for statement in &script.statements {
        match statement {
            Statement::Assign { assignment, .. } => visit(assignment),
            Statement::Output(stmt) => {
                for item in script.output(*stmt).map_or(&[][..], |o| o.items.as_slice()) {
                    if let Item::Assign { assignment, .. } = item {
                        visit(assignment);
                    }
                }
            }
            Statement::Assert { .. } => {}
        }
    }
    let other = last.flatten()?;
    symbols.lookup(&SymbolName::new(&other))
}

/// Evaluates a symbol or `.` assignment.
fn assign<F: crate::elf::read::ElfFormat>(
    ctx: &mut Context<'_, '_, F>,
    assignment: &crate::script::Assignment,
    wanted: &dyn Fn(&[u8]) -> bool,
) {
    if assignment.is_dot() {
        match eval_dot_assignment(assignment, ctx) {
            Ok(dot) => ctx.dot = dot,
            Err(error) => ctx.errors.push(error.to_string()),
        }
        return;
    }
    let provide = matches!(
        assignment.kind,
        AssignKind::Provide | AssignKind::ProvideHidden
    );
    if provide && !wanted(&assignment.target) {
        return;
    }
    match eval_symbol_assignment(assignment, ctx) {
        Ok(value) => {
            ctx.values.insert(assignment.target.clone(), value);
        }
        Err(error) => ctx.errors.push(error.to_string()),
    }
}

/// One output section after a pass.
struct Laid {
    members: Vec<ScriptMember>,
    data: Vec<(u64, Vec<u8>)>,
    fill: Option<Vec<u8>>,
    size: u64,
    align: u64,
}

/// Lays out the items of output statement `output` (script output `out`),
/// whose members by description are `list`.
fn lay_out_output<F: crate::elf::read::ElfFormat>(
    ctx: &mut Context<'_, '_, F>,
    output: &OutputStmt,
    (out, vma, attr_align, subalign): (u32, u64, u64, Option<u64>),
    list: &[(u16, SectionId)],
    files: &[ElfInput<'_, F>],
    sections: &Sections,
    wanted: &dyn Fn(&[u8]) -> bool,
) -> Laid {
    let abs = |ctx: &mut Context<'_, '_, F>, expr: &crate::script::Expr| match eval(expr, ctx) {
        Ok(value) => Some(value.resolve(ctx)),
        Err(error) => {
            ctx.errors.push(error.to_string());
            None
        }
    };
    let fill_of = |ctx: &mut Context<'_, '_, F>, fill: &Fill| match fill_pattern(fill, ctx) {
        Ok(pattern) => Some(pattern),
        Err(error) => {
            ctx.errors.push(error.to_string());
            None
        }
    };
    let mut fill = output.fill.as_ref().and_then(|f| fill_of(ctx, f));
    let mut laid = Laid {
        members: Vec::new(),
        data: Vec::new(),
        fill: None,
        size: 0,
        align: attr_align,
    };
    let previous_current = ctx.current;
    ctx.current = Some(out);
    ctx.dot = vma;
    for item in &output.items {
        match item {
            Item::Input { index, .. } => {
                for &(_, id) in list.iter().filter(|(sub, _)| sub == index) {
                    let Some((file_index, section_index)) = sections.locate(id) else {
                        continue;
                    };
                    let Some(section) = files
                        .get(file_index)
                        .and_then(|f| f.object.as_ref())
                        .and_then(|o| o.section(section_index))
                    else {
                        continue;
                    };
                    let align = subalign.unwrap_or(section.header.sh_addralign).max(1);
                    laid.align = laid.align.max(align);
                    let at = align_up(ctx.dot, align);
                    if at > ctx.dot
                        && let Some(pattern) = &fill
                    {
                        laid.data
                            .push((ctx.dot.wrapping_sub(vma), gap(pattern, ctx.dot, at)));
                    }
                    laid.members.push(ScriptMember {
                        file: u32::try_from(file_index).unwrap_or(NONE),
                        section: section_index,
                        offset: at.wrapping_sub(vma),
                    });
                    ctx.dot = at.wrapping_add(section.header.sh_size);
                }
            }
            Item::Assign { assignment, .. } => {
                let from = ctx.dot;
                assign(ctx, assignment, wanted);
                if ctx.dot < from {
                    ctx.errors.push(format!(
                        "cannot move location counter backwards (from {from:#x} to {:#x})",
                        ctx.dot
                    ));
                    ctx.dot = from;
                } else if ctx.dot > from
                    && let Some(pattern) = &fill
                {
                    laid.data
                        .push((from.wrapping_sub(vma), gap(pattern, from, ctx.dot)));
                }
            }
            Item::Data { size, expr, .. } => {
                let value = abs(ctx, expr).unwrap_or(0);
                let width = usize::try_from(size.bytes()).unwrap_or(8).min(8);
                laid.data.push((
                    ctx.dot.wrapping_sub(vma),
                    super::data_bytes::<F>(value, width),
                ));
                ctx.dot = ctx.dot.wrapping_add(size.bytes());
            }
            Item::Asciz(text) => {
                let mut bytes = text.clone();
                bytes.push(0);
                let len = u64::try_from(bytes.len()).unwrap_or(0);
                laid.data.push((ctx.dot.wrapping_sub(vma), bytes));
                ctx.dot = ctx.dot.wrapping_add(len);
            }
            Item::LinkerVersion => {
                let bytes = crate::elf::synth::comment();
                let len = u64::try_from(bytes.len()).unwrap_or(0);
                laid.data.push((ctx.dot.wrapping_sub(vma), bytes));
                ctx.dot = ctx.dot.wrapping_add(len);
            }
            Item::Fill(f) => fill = fill_of(ctx, f),
            Item::Assert { assert, .. } => {
                if abs(ctx, &assert.expr) == Some(0)
                    && let Err(error) = ctx.assertion_failed(&assert.message)
                {
                    ctx.errors.push(error.to_string());
                }
            }
        }
    }
    // Orphans of the statement's name come last, after its own items.
    let described = output
        .items
        .iter()
        .filter(|i| matches!(i, Item::Input { .. }))
        .count();
    for &(_, id) in list
        .iter()
        .filter(|(sub, _)| usize::from(*sub) >= described)
    {
        let Some((file_index, section_index)) = sections.locate(id) else {
            continue;
        };
        let Some(section) = files
            .get(file_index)
            .and_then(|f| f.object.as_ref())
            .and_then(|o| o.section(section_index))
        else {
            continue;
        };
        let align = subalign.unwrap_or(section.header.sh_addralign).max(1);
        laid.align = laid.align.max(align);
        let at = align_up(ctx.dot, align);
        laid.members.push(ScriptMember {
            file: u32::try_from(file_index).unwrap_or(NONE),
            section: section_index,
            offset: at.wrapping_sub(vma),
        });
        ctx.dot = at.wrapping_add(section.header.sh_size);
    }
    laid.fill = fill;
    laid.size = ctx.dot.wrapping_sub(vma);
    ctx.current = previous_current;
    laid
}

/// `pattern` repeated over `from..to`, aligned as GNU ld repeats a fill
/// from the start of each gap.
fn gap(pattern: &[u8], from: u64, to: u64) -> Vec<u8> {
    let len = usize::try_from(to.saturating_sub(from)).unwrap_or(0);
    if pattern.is_empty() {
        return vec![0; len];
    }
    pattern.iter().copied().cycle().take(len).collect()
}

/// The statement index of each script output.
fn output_of_stmt_list(output_of_stmt: &[u32], outputs: usize) -> Vec<u32> {
    let mut list = vec![NONE; outputs];
    for (stmt, &out) in output_of_stmt.iter().enumerate() {
        if let Some(slot) = list.get_mut(out as usize) {
            *slot = u32::try_from(stmt).unwrap_or(NONE);
        }
    }
    list
}
