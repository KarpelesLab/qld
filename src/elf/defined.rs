//! Linker-defined symbols.
//!
//! GNU ld's default script defines symbols that mark layout boundaries
//! (`_end`, `__bss_start`, `__init_array_start`, …) with `PROVIDE`
//! semantics: only when something references them and nothing defines them.
//! qld does the same after resolution, by replacing the (undefined or lazy)
//! definition with a [`LINKER_FILE`] definition whose index is a slot in
//! [`LinkerSymbols`]. `__start_SEC`/`__stop_SEC` are defined for every output
//! section whose name is a C identifier. Values are computed after layout.
//!
//! `--defsym name=expr` takes any linker script expression, as in GNU ld.
//! Under a linker script the script engine evaluates it (it is an
//! assignment before the script's statements); otherwise
//! [`evaluate_defsyms`] does after layout, with GNU ld's rules for which
//! values are absolute. A symbol assigned from an expression that reads one
//! symbol gets that symbol's type ([`linker_type`]).

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::LinkOptions;
use crate::error::{Error, Result};
use crate::ids::SymbolId;
use crate::script::{
    Assignment, EvalContext, EvalError, Value as ExprValue, ValueSection, eval_symbol_assignment,
};
use crate::symbols::{
    Definition, DefinitionKind, InputPosition, SymbolFlags, SymbolName, SymbolTable,
};

use super::inputs::{DefsymExpr, ElfInput, parse_defsym};
use super::place::Placement;
use super::refs::LINKER_FILE;
use super::rules::is_c_identifier;
use super::script_layout::{ResolvedSymbols, ScriptPlacement, SymbolDef};

/// What a linker-defined symbol's value is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    /// The ELF header (`__ehdr_start`).
    EhdrStart,
    /// The start of the image (`__executable_start`).
    ExecutableStart,
    /// End of the text segment (`_etext`, `etext`, `__etext`).
    Etext,
    /// End of initialized data (`_edata`, `edata`).
    Edata,
    /// Start of `.bss` (`__bss_start`).
    BssStart,
    /// End of the image (`_end`, `end`).
    End,
    /// Start of the named output section of the default rules.
    SectionStart(&'static str),
    /// End of the named output section of the default rules.
    SectionEnd(&'static str),
    /// Start of output section (by placement index): `__start_SEC`.
    OutputStart(u32),
    /// End of output section (by placement index): `__stop_SEC`.
    OutputEnd(u32),
    /// `_GLOBAL_OFFSET_TABLE_`.
    GotBase,
    /// `__rela_iplt_start`.
    RelaIpltStart,
    /// `__rela_iplt_end`.
    RelaIpltEnd,
    /// `_DYNAMIC`: the `.dynamic` section.
    Dynamic,
    /// `_TLS_MODULE_BASE_`: the start of the output's TLS block, which
    /// local-dynamic TLS descriptor code takes offsets from.
    TlsModuleBase,
    /// RISC-V `__global_pointer$`: 0x800 past the start of `.sdata`, or of
    /// the image when there is none (lld's definition).
    GlobalPointer,
    /// `--defsym`, by index in the options.
    Defsym(usize),
    /// A linker script symbol, by slot in the script placement's symbol
    /// names.
    Script {
        /// The slot.
        slot: u32,
        /// `HIDDEN` or `PROVIDE_HIDDEN`.
        hidden: bool,
    },
}

/// Whether a symbol gets hidden visibility (`PROVIDE_HIDDEN`).
#[must_use]
pub fn is_hidden(value: Value) -> bool {
    matches!(
        value,
        Value::EhdrStart
            | Value::SectionStart(_)
            | Value::SectionEnd(_)
            | Value::RelaIpltStart
            | Value::RelaIpltEnd
            | Value::GotBase
            | Value::Dynamic
            | Value::TlsModuleBase
            | Value::Script { hidden: true, .. }
    )
}

const FIXED: &[(&str, Value)] = &[
    ("__ehdr_start", Value::EhdrStart),
    ("__executable_start", Value::ExecutableStart),
    ("_etext", Value::Etext),
    ("etext", Value::Etext),
    ("__etext", Value::Etext),
    ("_edata", Value::Edata),
    ("edata", Value::Edata),
    ("__bss_start", Value::BssStart),
    ("_end", Value::End),
    ("end", Value::End),
    (
        "__preinit_array_start",
        Value::SectionStart(".preinit_array"),
    ),
    ("__preinit_array_end", Value::SectionEnd(".preinit_array")),
    ("__init_array_start", Value::SectionStart(".init_array")),
    ("__init_array_end", Value::SectionEnd(".init_array")),
    ("__fini_array_start", Value::SectionStart(".fini_array")),
    ("__fini_array_end", Value::SectionEnd(".fini_array")),
    ("__tdata_start", Value::SectionStart(".tdata")),
    ("_GLOBAL_OFFSET_TABLE_", Value::GotBase),
    // PowerPC64's TOC pointer, which takes the place of the GOT base.
    (".TOC.", Value::GotBase),
    ("__rela_iplt_start", Value::RelaIpltStart),
    ("__rela_iplt_end", Value::RelaIpltEnd),
    // The same bounds where `IRELATIVE` relocations are `SHT_REL` (i386
    // glibc's static startup reads these names).
    ("__rel_iplt_start", Value::RelaIpltStart),
    ("__rel_iplt_end", Value::RelaIpltEnd),
    ("_DYNAMIC", Value::Dynamic),
    // Arm: the exception index, which unwinders read in static
    // executables.
    ("__exidx_start", Value::SectionStart(".ARM.exidx")),
    ("__exidx_end", Value::SectionEnd(".ARM.exidx")),
    ("_TLS_MODULE_BASE_", Value::TlsModuleBase),
];

/// Symbols an executable always defines, as GNU ld's default script
/// assigns them unconditionally; they are exported with `--export-dynamic`.
pub const ALWAYS_DEFINED: &[&str] = &["_edata", "__bss_start", "_end"];

/// Boundary symbols GNU ld's AArch64 and Arm default scripts assign
/// besides the common ones (`__bss_start__ = .;` before `.bss`,
/// `_bss_end__` and `__bss_end__` after it, `__end__` with `_end`). qld's
/// `.bss` ends at `_end`, so the last three take its value.
const AARCH64_EXTRA: &[(&str, Value)] = &[
    ("__bss_start__", Value::BssStart),
    ("_bss_end__", Value::End),
    ("__bss_end__", Value::End),
    ("__end__", Value::End),
];

/// The linker symbols a link defines even when nothing refers to them, as
/// GNU ld does: its default scripts for executables assign `_edata`,
/// `__bss_start` and `_end` outside `PROVIDE` (on AArch64 also the
/// `AARCH64_EXTRA` boundary symbols), and its ELF backend defines
/// `_DYNAMIC` whenever it creates dynamic sections. They are in `.symtab`
/// (`_DYNAMIC` as a local), and in `.dynsym` only when exported.
#[must_use]
pub fn always_defined<F: crate::elf::read::ElfFormat>(
    mode: super::export::Mode,
    script: bool,
    files: &[ElfInput<'_, F>],
) -> Vec<&'static str> {
    let mut names = Vec::new();
    if mode.executable() && !script {
        names.extend_from_slice(ALWAYS_DEFINED);
        if matches!(
            super::arch::Arch::of_files(files),
            Some(super::arch::Arch::AArch64 | super::arch::Arch::Arm)
        ) {
            names.extend(AARCH64_EXTRA.iter().map(|&(name, _)| name));
        }
    }
    if mode.dynamic {
        names.push("_DYNAMIC");
    }
    names
}

/// The linker-defined symbols of a link.
#[derive(Debug, Default)]
pub struct LinkerSymbols {
    /// `(symbol, value)`, in slot order.
    pub entries: Vec<(SymbolId, Value)>,
    /// Output sections referenced by `__start_`/`__stop_` symbols, which
    /// GC keeps.
    pub start_stop_outputs: Vec<u32>,
    /// `(symbol, source)`, sorted: the symbol whose type a script or
    /// `--defsym` symbol copies ([`linker_type`]).
    pub type_sources: Vec<(SymbolId, SymbolId)>,
}

impl LinkerSymbols {
    /// The value kind of slot `index`.
    #[must_use]
    pub fn get(&self, index: u32) -> Option<(SymbolId, Value)> {
        self.entries.get(index as usize).copied()
    }

    /// Whether `_GLOBAL_OFFSET_TABLE_` is defined.
    #[must_use]
    pub fn uses_got_base(&self) -> bool {
        self.entries.iter().any(|(_, v)| *v == Value::GotBase)
    }
}

fn wanted(symbols: &SymbolTable<'_>, id: SymbolId) -> bool {
    matches!(
        symbols.definition_kind(id),
        DefinitionKind::Undefined | DefinitionKind::Lazy | DefinitionKind::Shared
    ) && symbols.flags(id).intersects(
        super::dso::REF_REGULAR | SymbolFlags::REFERENCED | SymbolFlags::WEAK_REFERENCED,
    ) && (symbols.definition_kind(id) != DefinitionKind::Shared
        || symbols.flags(id).contains(super::dso::REF_REGULAR))
}

/// Defines the linker symbols that are referenced and not otherwise defined.
#[must_use]
///
/// `dynamic` says the output is dynamic: `__rela_iplt_start` and
/// `__rela_iplt_end` then stay undefined, as in GNU ld's dynamic scripts,
/// since the dynamic relocation code applies `IRELATIVE` relocations.
/// Names in `always` are defined even when nothing refers to them.
pub fn register<F: crate::elf::read::ElfFormat>(
    symbols: &SymbolTable<'_>,
    files: &[ElfInput<'_, F>],
    placement: &Placement<'_>,
    options: &LinkOptions,
    dynamic: bool,
    always: &[&str],
) -> LinkerSymbols {
    let mut result = LinkerSymbols::default();
    let define = |id: SymbolId, value: Value, result: &mut LinkerSymbols| {
        let slot = u32::try_from(result.entries.len()).unwrap_or(u32::MAX);
        symbols.replace_definition(
            id,
            &Definition {
                kind: DefinitionKind::Regular,
                file: LINKER_FILE,
                index: slot,
                position: InputPosition::from_raw(u64::MAX),
                aux: 0,
            },
        );
        result.entries.push((id, value));
    };
    let script = placement.script.as_deref();
    let boundaries = script.is_none()
        && matches!(
            super::arch::Arch::of_files(files),
            Some(super::arch::Arch::AArch64 | super::arch::Arch::Arm)
        );
    let extra: &[(&str, Value)] = if boundaries { AARCH64_EXTRA } else { &[] };
    for &(name, value) in FIXED.iter().chain(extra) {
        if dynamic && matches!(value, Value::RelaIpltStart | Value::RelaIpltEnd) {
            continue;
        }
        // Under a linker script, only the symbols the ELF backend itself
        // defines; the others come from the default script.
        if script.is_some() && !matches!(value, Value::EhdrStart | Value::GotBase | Value::Dynamic)
        {
            continue;
        }
        if !dynamic && value == Value::Dynamic {
            continue;
        }
        if let Some(id) = symbols.lookup(&SymbolName::new(name.as_bytes()))
            && (wanted(symbols, id)
                || (always.contains(&name)
                    && matches!(
                        symbols.definition_kind(id),
                        DefinitionKind::Undefined | DefinitionKind::Lazy | DefinitionKind::Shared
                    )))
        {
            define(id, value, &mut result);
        }
    }

    // RISC-V executables: `__global_pointer$`, which the C runtime loads
    // into `gp`.
    if options.kind != crate::args::OutputKind::Shared
        && super::arch::Arch::of_files(files) == Some(super::arch::Arch::RiscV64)
        && let Some(id) = symbols.lookup(&SymbolName::new(b"__global_pointer$"))
        && wanted(symbols, id)
    {
        define(id, Value::GlobalPointer, &mut result);
    }

    // __start_SEC / __stop_SEC.
    let mut start_stop: Vec<(SymbolId, bool, &[u8])> = symbols
        .names()
        .par_iter()
        .enumerate()
        .filter_map(|(index, name)| {
            if name.version().is_some() {
                return None;
            }
            let bytes = name.bytes();
            let (start, section) = match bytes.strip_prefix(b"__start_") {
                Some(rest) => (true, rest),
                None => (false, bytes.strip_prefix(b"__stop_")?),
            };
            let id = SymbolId::new(index);
            (is_c_identifier(section) && wanted(symbols, id)).then_some((id, start, section))
        })
        .collect();
    start_stop.sort_unstable_by_key(|(id, _, _)| *id);
    for (id, start, section) in start_stop {
        let output = placement
            .outputs
            .iter()
            .position(|o| o.name == section)
            .and_then(|o| u32::try_from(o).ok());
        if let Some(output) = output {
            let value = if start {
                Value::OutputStart(output)
            } else {
                Value::OutputEnd(output)
            };
            define(id, value, &mut result);
            result.start_stop_outputs.push(output);
        }
    }
    result.start_stop_outputs.sort_unstable();
    result.start_stop_outputs.dedup();

    if let Some(script) = script {
        register_script(symbols, files, script, &mut result, &define);
    }

    // --defsym: resolution already made the internal file the definition.
    // (Under a linker script, the script evaluates them.)
    for (index, (name, expr)) in options.defsym.iter().enumerate() {
        if let Some(id) = symbols.lookup(&SymbolName::new(name.as_bytes()))
            && symbols.definition(id).file.index() == 0
        {
            let assignment = defsym_assignment(name, expr).ok();
            if assignment
                .as_ref()
                .is_none_or(|a| defsym_is_absolute(symbols, files, a))
            {
                symbols.set_flags(id, ABSOLUTE);
            }
            if let Some(source) = assignment
                .as_ref()
                .and_then(|a| a.expr.type_source())
                .and_then(|n| symbols.lookup(&SymbolName::new(n)))
            {
                result.type_sources.push((id, source));
            }
            result.entries.push((id, Value::Defsym(index)));
        }
    }
    result.type_sources.sort_unstable();
    result.type_sources.dedup_by_key(|(id, _)| *id);
    result
}

/// Defines the symbols linker scripts assign and records where the symbols
/// they read are defined.
fn register_script<F: crate::elf::read::ElfFormat>(
    symbols: &SymbolTable<'_>,
    files: &[ElfInput<'_, F>],
    script: &ScriptPlacement,
    result: &mut LinkerSymbols,
    define: &dyn Fn(SymbolId, Value, &mut LinkerSymbols),
) {
    let mut needed = Vec::with_capacity(script.symbol_names.len());
    let mut hidden = Vec::with_capacity(script.symbol_names.len());
    for (slot, name) in script.symbol_names.iter().enumerate() {
        let (provide, is_hidden) = script
            .symbol_kinds
            .get(slot)
            .copied()
            .unwrap_or((false, false));
        hidden.push(is_hidden);
        let id = symbols.lookup(&SymbolName::new(name));
        let apply = match id {
            Some(id) if provide => {
                matches!(
                    symbols.definition_kind(id),
                    DefinitionKind::Undefined | DefinitionKind::Lazy | DefinitionKind::Shared
                ) && (wanted(symbols, id) || script.referenced.iter().any(|r| r == name))
            }
            Some(_) => true,
            None => !provide,
        };
        needed.push(apply);
        if apply && let Some(id) = id {
            if let Some(source) = script
                .type_sources
                .get(slot)
                .and_then(Option::as_deref)
                .and_then(|n| symbols.lookup(&SymbolName::new(n)))
            {
                result.type_sources.push((id, source));
            }
            let slot = u32::try_from(slot).unwrap_or(u32::MAX);
            define(
                id,
                Value::Script {
                    slot,
                    hidden: is_hidden,
                },
                result,
            );
        }
    }
    let mut defs: Vec<(Vec<u8>, SymbolDef)> = Vec::with_capacity(script.referenced.len());
    for name in &script.referenced {
        let def = match symbols.lookup(&SymbolName::new(name)) {
            Some(id) => symbol_def(symbols, files, id),
            None => SymbolDef::Undefined,
        };
        defs.push((name.clone(), def));
    }
    defs.sort_by(|a, b| a.0.cmp(&b.0));
    let _ = script.resolved.set(ResolvedSymbols {
        defs,
        needed,
        hidden,
    });
}

/// Where symbol `id` is defined, for script expressions.
fn symbol_def<F: crate::elf::read::ElfFormat>(
    symbols: &SymbolTable<'_>,
    files: &[ElfInput<'_, F>],
    id: SymbolId,
) -> SymbolDef {
    use crate::elf::read::SectionIndex;
    let def = symbols.definition(id);
    match def.kind {
        DefinitionKind::Undefined | DefinitionKind::Lazy => SymbolDef::Undefined,
        DefinitionKind::Shared | DefinitionKind::Common => SymbolDef::Other,
        DefinitionKind::Regular | DefinitionKind::Weak => {
            if def.file == LINKER_FILE {
                return SymbolDef::Linker;
            }
            let file = def.file.index();
            let Some(object) = files.get(file).and_then(|f| f.object.as_ref()) else {
                return SymbolDef::Other;
            };
            let table = object.elf.symbols();
            let Some(index) = (def.index as usize).checked_add(object.first_global) else {
                return SymbolDef::Undefined;
            };
            let Some(raw) = table.get_raw(index) else {
                return SymbolDef::Undefined;
            };
            match table.section(index, &raw) {
                Ok(SectionIndex::Section(section)) => SymbolDef::Section {
                    file,
                    section,
                    value: raw.st_value,
                },
                Ok(SectionIndex::Absolute) => SymbolDef::Absolute(raw.st_value),
                _ => SymbolDef::Other,
            }
        }
    }
}

/// The section header index a linker-defined symbol belongs to, when its
/// value alone does not tell: a script symbol assigned relative to an
/// output section, or `__start_SEC`/`__stop_SEC`, belongs to that section
/// even at its end, where the next section may start.
#[must_use]
pub fn linker_shndx<F: crate::elf::read::ElfFormat>(
    addresses: &super::values::Addresses<'_, '_, F>,
    linker: &LinkerSymbols,
    id: SymbolId,
) -> Option<u16> {
    let layout = addresses.layout;
    let (_, value) = linker.entries.iter().find(|(i, _)| *i == id)?;
    let position = match *value {
        Value::Script { slot, .. } => layout.script_symbols.get(slot as usize)?.section?,
        Value::OutputStart(output) | Value::OutputEnd(output) => {
            let position = layout.output_places.get(output as usize)?.2;
            (position != super::sections::NONE).then_some(position)?
        }
        _ => return None,
    };
    u16::try_from(position.checked_add(1)?)
        .ok()
        .filter(|&i| i < crate::elf::read::consts::SHN_LORESERVE)
}

/// Backend flag: the symbol's value is an absolute number, not an address
/// in the image (`--defsym name=0x1234`), so position-independent output
/// does not relocate it.
pub const ABSOLUTE: SymbolFlags = SymbolFlags::backend(7);

/// The parsed `--defsym` expression for slot value `Defsym(index)`, when
/// it is a number or `symbol+offset`.
#[must_use]
pub fn defsym_expr(options: &LinkOptions, index: usize) -> Option<DefsymExpr> {
    options.defsym.get(index).and_then(|(_, e)| parse_defsym(e))
}

// ---------------------------------------------------------------------------
// `--defsym` expressions.
// ---------------------------------------------------------------------------

/// The script assignment `--defsym name=expr` stands for: GNU ld parses it
/// as a linker script assignment, so any expression is allowed.
///
/// # Errors
///
/// [`Error::Script`] for a syntax error.
pub fn defsym_assignment(name: &str, expr: &str) -> Result<Assignment> {
    let text = format!("{name}={expr}");
    crate::script::parse_defsym(text.as_bytes()).map_err(|e| Error::Script(Box::new(e)))
}

/// The symbols whose values a `--defsym` expression reads: the linker's
/// references (archive members that define them are loaded, and garbage
/// collection keeps their sections). Malformed expressions read nothing;
/// they are reported before the link starts.
#[must_use]
pub fn defsym_references(name: &str, expr: &str) -> Vec<Vec<u8>> {
    let mut names = Vec::new();
    if let Ok(assignment) = defsym_assignment(name, expr) {
        assignment.expr.for_each_value_symbol(&mut |symbol| {
            if symbol != name.as_bytes() && !names.iter().any(|n: &Vec<u8>| n == symbol) {
                names.push(symbol.to_vec());
            }
        });
    }
    names
}

/// The type a linker-defined symbol gets in symbol tables: the type of the
/// symbol its assignment copies (`sym = other;`, or any expression reading
/// one symbol, as GNU ld's `bfd_copy_link_hash_symbol_type`), or
/// `STT_NOTYPE`.
#[must_use]
pub fn linker_type<F: crate::elf::read::ElfFormat>(
    refs: &super::refs::Refs<'_, '_, F>,
    linker: &LinkerSymbols,
    id: SymbolId,
) -> u8 {
    use crate::elf::read::consts::{STT_NOTYPE, STT_OBJECT, STT_TLS};
    // GNU ld's ELF backend defines these as objects.
    if linker
        .entries
        .iter()
        .any(|(i, v)| *i == id && matches!(v, Value::GotBase | Value::Dynamic))
    {
        return STT_OBJECT;
    }
    if linker
        .entries
        .iter()
        .any(|(i, v)| *i == id && *v == Value::TlsModuleBase)
    {
        return STT_TLS;
    }
    let mut id = id;
    // A chain of copies ends at an input's symbol; cycles stop.
    for _ in 0..8 {
        let Ok(at) = linker.type_sources.binary_search_by_key(&id, |(i, _)| *i) else {
            return STT_NOTYPE;
        };
        let Some(&(_, source)) = linker.type_sources.get(at) else {
            return STT_NOTYPE;
        };
        let target = refs.global_target(source, true);
        match target.def {
            super::refs::Def::Linker(_) => id = source,
            _ => return target.raw.map_or(STT_NOTYPE, |raw| raw.kind()),
        }
    }
    STT_NOTYPE
}

/// A section handle for defsym evaluation: an output section by position
/// in the layout, or "the image" for linker-defined addresses whose section
/// the symbol table works out from the address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DefsymSection {
    Output(u32),
    Image,
}

/// What `--defsym` expressions read after layout.
struct DefsymContext<'c, 'x, 'a, F: crate::elf::read::ElfFormat = crate::elf::read::Elf64Le> {
    refs: &'c super::refs::Refs<'x, 'a, F>,
    layout: &'c super::layout::Layout<'a>,
    options: &'c LinkOptions,
    globals: &'c [u64],
    /// Defsym results so far (this pass, else the previous one).
    values: &'c [(SymbolId, ExprValue<DefsymSection>)],
}

impl<F: crate::elf::read::ElfFormat> DefsymContext<'_, '_, '_, F> {
    fn position_named(&self, name: &[u8]) -> Option<u32> {
        let position = self.layout.sections.iter().position(|s| s.name == name)?;
        u32::try_from(position).ok()
    }
}

impl<F: crate::elf::read::ElfFormat> EvalContext for DefsymContext<'_, '_, '_, F> {
    type Section = DefsymSection;

    fn section_vma(&self, section: DefsymSection) -> u64 {
        match section {
            DefsymSection::Output(position) => self
                .layout
                .sections
                .get(position as usize)
                .map_or(0, |s| s.addr),
            DefsymSection::Image => 0,
        }
    }

    fn dot(&self) -> std::result::Result<u64, EvalError> {
        Ok(0)
    }

    fn symbol(&mut self, name: &[u8]) -> std::result::Result<ExprValue<DefsymSection>, EvalError> {
        use super::refs::Def;
        let Some(id) = self.refs.symbols.lookup(&SymbolName::new(name)) else {
            return Err(EvalError::UndefinedSymbol(name.to_vec()));
        };
        if let Some((_, value)) = self.values.iter().find(|(i, _)| *i == id) {
            return Ok(*value);
        }
        let address = self.globals.get(id.index()).copied().unwrap_or(0);
        let target = self.refs.global_target(id, true);
        Ok(match target.def {
            Def::Section { file, section, .. } => {
                let position = self
                    .refs
                    .sections
                    .id(file, section)
                    .and_then(|s| self.refs.sections.resolve(s))
                    .and_then(|s| self.layout.section_shndx.get(s.index()).copied())
                    .and_then(|shndx| shndx.checked_sub(1))
                    .filter(|&p| (p as usize) < self.layout.sections.len());
                match position {
                    Some(position) => {
                        let base = self.section_vma(DefsymSection::Output(position));
                        ExprValue::relative(
                            DefsymSection::Output(position),
                            address.wrapping_sub(base),
                        )
                    }
                    None => ExprValue::absolute(address),
                }
            }
            Def::Absolute(value) => ExprValue::absolute(value),
            Def::Linker(_) if self.refs.symbols.flags(id).contains(ABSOLUTE) => {
                ExprValue::absolute(address)
            }
            Def::Linker(_) | Def::Common(_) | Def::Shared(_) => {
                ExprValue::relative(DefsymSection::Image, address)
            }
            // Undefined symbols are reported as undefined references.
            Def::Undefined { .. } => ExprValue::absolute(0),
        })
    }

    fn is_defined(&mut self, name: &[u8]) -> bool {
        self.refs
            .symbols
            .lookup(&SymbolName::new(name))
            .is_some_and(|id| {
                matches!(
                    self.refs.symbols.definition_kind(id),
                    DefinitionKind::Regular | DefinitionKind::Weak | DefinitionKind::Common
                )
            })
    }

    fn section_addr(
        &mut self,
        name: &[u8],
    ) -> std::result::Result<ExprValue<DefsymSection>, EvalError> {
        self.position_named(name)
            .map(|p| ExprValue::relative(DefsymSection::Output(p), 0))
            .ok_or_else(|| EvalError::UndefinedSection(name.to_vec()))
    }

    fn section_load_addr(&mut self, name: &[u8]) -> std::result::Result<u64, EvalError> {
        self.position_named(name)
            .and_then(|p| self.layout.sections.get(p as usize))
            .map(|s| s.lma)
            .ok_or_else(|| EvalError::UndefinedSection(name.to_vec()))
    }

    fn section_size(&mut self, name: &[u8]) -> std::result::Result<u64, EvalError> {
        // GNU ld gives 0 for sections that do not exist.
        Ok(self
            .position_named(name)
            .and_then(|p| self.layout.sections.get(p as usize))
            .map_or(0, |s| s.size))
    }

    fn section_alignment(&mut self, name: &[u8]) -> std::result::Result<u64, EvalError> {
        self.position_named(name)
            .and_then(|p| self.layout.sections.get(p as usize))
            .map(|s| s.align)
            .ok_or_else(|| EvalError::UndefinedSection(name.to_vec()))
    }

    fn max_page_size(&self) -> std::result::Result<u64, EvalError> {
        Ok(self.options.max_page_size.unwrap_or(0x1000))
    }

    fn common_page_size(&self) -> std::result::Result<u64, EvalError> {
        Ok(self.options.common_page_size.unwrap_or(0x1000))
    }
}

/// Evaluates the `--defsym` expressions of `defsyms` (symbol, index in the
/// options) after layout, into `globals`, as GNU ld evaluates them: in
/// command-line order before the default script's `SECTIONS`, with `.` at
/// 0, repeated so that forward references to later `--defsym`s settle.
/// Symbols whose value is absolute (every expression but a lone symbol,
/// `.` or `ADDR`) get [`ABSOLUTE`].
pub fn evaluate_defsyms<'a, F: crate::elf::read::ElfFormat>(
    globals: &mut [u64],
    refs: &super::refs::Refs<'_, 'a, F>,
    layout: &super::layout::Layout<'a>,
    options: &LinkOptions,
    defsyms: &[(SymbolId, usize)],
) {
    let assignments: Vec<(SymbolId, Option<Assignment>)> = defsyms
        .iter()
        .map(|&(id, index)| {
            let assignment = options
                .defsym
                .get(index)
                .and_then(|(name, expr)| defsym_assignment(name, expr).ok());
            (id, assignment)
        })
        .collect();
    let mut values: Vec<(SymbolId, ExprValue<DefsymSection>)> = Vec::new();
    for _pass in 0..=assignments.len().min(8) {
        let mut next: Vec<(SymbolId, ExprValue<DefsymSection>)> = Vec::new();
        for (id, assignment) in &assignments {
            let value = match assignment {
                Some(assignment) => {
                    // This pass's values first, then the previous pass's.
                    let mut seen = next.clone();
                    seen.extend(
                        values
                            .iter()
                            .filter(|(i, _)| !next.iter().any(|(n, _)| n == i)),
                    );
                    let mut context = DefsymContext {
                        refs,
                        layout,
                        options,
                        globals,
                        values: &seen,
                    };
                    eval_symbol_assignment(assignment, &mut context)
                        .unwrap_or_else(|_| ExprValue::absolute(0))
                }
                None => ExprValue::absolute(0),
            };
            next.retain(|(i, _)| i != id);
            next.push((*id, value));
        }
        let settled = next == values;
        values = next;
        if settled {
            break;
        }
    }
    let context = DefsymContext {
        refs,
        layout,
        options,
        globals,
        values: &[],
    };
    let resolved: Vec<(SymbolId, u64, bool)> = values
        .iter()
        .map(|(id, value)| {
            let absolute = !matches!(value.section, ValueSection::Relative(_));
            (*id, value.resolve(&context), absolute)
        })
        .collect();
    for (id, address, absolute) in resolved {
        if absolute {
            refs.symbols.set_flags(id, ABSOLUTE);
        } else {
            refs.symbols.clear_flags(id, ABSOLUTE);
        }
        if let Some(slot) = globals.get_mut(id.index()) {
            *slot = address;
        }
    }
}

/// Whether a `--defsym` expression gives an absolute value, decided before
/// layout (the relocation scan needs it) from how its symbols are defined:
/// GNU ld's rules make every expression but a lone symbol, `.`, `ADDR` and
/// the like absolute outside output sections.
fn defsym_is_absolute<F: crate::elf::read::ElfFormat>(
    symbols: &SymbolTable<'_>,
    files: &[ElfInput<'_, F>],
    assignment: &Assignment,
) -> bool {
    /// Values are irrelevant here, only the sections results end up in.
    struct Classify<'c, 's, 'f, F: crate::elf::read::ElfFormat = crate::elf::read::Elf64Le> {
        symbols: &'c SymbolTable<'s>,
        files: &'c [ElfInput<'f, F>],
    }
    impl<F: crate::elf::read::ElfFormat> EvalContext for Classify<'_, '_, '_, F> {
        type Section = ();
        fn section_vma(&self, _section: ()) -> u64 {
            0
        }
        fn dot(&self) -> std::result::Result<u64, EvalError> {
            Ok(0)
        }
        fn symbol(&mut self, name: &[u8]) -> std::result::Result<ExprValue<()>, EvalError> {
            let Some(id) = self.symbols.lookup(&SymbolName::new(name)) else {
                return Ok(ExprValue::absolute(0));
            };
            Ok(match symbol_def(self.symbols, self.files, id) {
                SymbolDef::Absolute(_) | SymbolDef::Undefined => ExprValue::absolute(0),
                SymbolDef::Linker if self.symbols.flags(id).contains(ABSOLUTE) => {
                    ExprValue::absolute(0)
                }
                _ => ExprValue::relative((), 0),
            })
        }
        fn is_defined(&mut self, name: &[u8]) -> bool {
            self.symbols
                .lookup(&SymbolName::new(name))
                .is_some_and(|id| {
                    matches!(
                        self.symbols.definition_kind(id),
                        DefinitionKind::Regular | DefinitionKind::Weak | DefinitionKind::Common
                    )
                })
        }
        fn section_addr(&mut self, _name: &[u8]) -> std::result::Result<ExprValue<()>, EvalError> {
            Ok(ExprValue::relative((), 0))
        }
        fn section_load_addr(&mut self, _name: &[u8]) -> std::result::Result<u64, EvalError> {
            Ok(0)
        }
        fn section_size(&mut self, _name: &[u8]) -> std::result::Result<u64, EvalError> {
            Ok(0)
        }
        fn section_alignment(&mut self, _name: &[u8]) -> std::result::Result<u64, EvalError> {
            Ok(1)
        }
        fn max_page_size(&self) -> std::result::Result<u64, EvalError> {
            Ok(0x1000)
        }
        fn common_page_size(&self) -> std::result::Result<u64, EvalError> {
            Ok(0x1000)
        }
    }
    let mut context = Classify { symbols, files };
    eval_symbol_assignment(assignment, &mut context)
        .map_or(true, |v| !matches!(v.section, ValueSection::Relative(())))
}
