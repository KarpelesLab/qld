//! Linker-defined symbols.
//!
//! GNU ld's default script defines symbols that mark layout boundaries
//! (`_end`, `__bss_start`, `__init_array_start`, …) with `PROVIDE`
//! semantics: only when something references them and nothing defines them.
//! qld does the same after resolution, by replacing the (undefined or lazy)
//! definition with a [`LINKER_FILE`] definition whose index is a slot in
//! [`LinkerSymbols`]. `__start_SEC`/`__stop_SEC` are defined for every output
//! section whose name is a C identifier. Values are computed after layout.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::LinkOptions;
use crate::ids::SymbolId;
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
    ("__rela_iplt_start", Value::RelaIpltStart),
    ("__rela_iplt_end", Value::RelaIpltEnd),
    ("_DYNAMIC", Value::Dynamic),
];

/// Symbols an executable always defines, as GNU ld's default script
/// assigns them unconditionally; they are exported with `--export-dynamic`.
pub const ALWAYS_DEFINED: &[&str] = &["_edata", "__bss_start", "_end"];

/// The linker-defined symbols of a link.
#[derive(Debug, Default)]
pub struct LinkerSymbols {
    /// `(symbol, value)`, in slot order.
    pub entries: Vec<(SymbolId, Value)>,
    /// Output sections referenced by `__start_`/`__stop_` symbols, which
    /// GC keeps.
    pub start_stop_outputs: Vec<u32>,
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
pub fn register(
    symbols: &SymbolTable<'_>,
    files: &[ElfInput<'_>],
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
    for &(name, value) in FIXED {
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
    for (index, (name, expr)) in options.defsym.iter().enumerate() {
        if let Some(id) = symbols.lookup(&SymbolName::new(name.as_bytes()))
            && symbols.definition(id).file.index() == 0
        {
            if let Some(DefsymExpr::Absolute(_)) = parse_defsym(expr) {
                symbols.set_flags(id, ABSOLUTE);
            }
            result.entries.push((id, Value::Defsym(index)));
        }
    }
    result
}

/// Defines the symbols linker scripts assign and records where the symbols
/// they read are defined.
fn register_script(
    symbols: &SymbolTable<'_>,
    files: &[ElfInput<'_>],
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
fn symbol_def(symbols: &SymbolTable<'_>, files: &[ElfInput<'_>], id: SymbolId) -> SymbolDef {
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
pub fn linker_shndx(
    addresses: &super::values::Addresses<'_, '_>,
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

/// The parsed `--defsym` expression for slot value `Defsym(index)`.
#[must_use]
pub fn defsym_expr(options: &LinkOptions, index: usize) -> Option<DefsymExpr> {
    options.defsym.get(index).and_then(|(_, e)| parse_defsym(e))
}
