//! The export directory (`.edata`).
//!
//! Exports come from four places, in this order of precedence: a `.def`
//! file, `-export:` directives (which is how `__declspec(dllexport)` reaches
//! the linker), `-export:` on the command line, and `--export-all-symbols`.
//! As GNU `ld` does for MinGW, a DLL with none of the first three exports
//! everything it defines, minus the runtime symbols in [`is_filtered`].
//!
//! The table is laid out as the PE specification describes: an
//! `IMAGE_EXPORT_DIRECTORY`, the export address table indexed by ordinal, the
//! name pointer table sorted by name so the loader can binary-search it, the
//! name ordinal table, and the strings. Its size depends only on the names,
//! so it is known before layout; the RVAs are filled in afterwards.

#![deny(clippy::arithmetic_side_effects)]

use crate::diag::{Diagnostic, DiagnosticSink};
use crate::ids::SymbolId;
use crate::symbols::{DefinitionKind, SymbolName, SymbolTable};

use super::directives::ExportRequest;
use super::inputs::CoffInput;
use super::object::GlobalKind;
use super::options::PeOptions;
use super::reloc::{Addresses, Value};

/// Size of `IMAGE_EXPORT_DIRECTORY`.
pub const EXPORT_DIRECTORY_SIZE: u32 = 40;

/// One symbol the image exports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    /// The name the DLL exports it under.
    pub name: Vec<u8>,
    /// The symbol it refers to, unless it is a forwarder.
    pub symbol: Option<Vec<u8>>,
    /// `name=dll.function`: the loader resolves it in another DLL.
    pub forwarder: Option<Vec<u8>>,
    /// The ordinal, assigned once every export is known.
    pub ordinal: u16,
    /// `NONAME`: reachable by ordinal only.
    pub noname: bool,
    /// `DATA`: the import library gives it no thunk.
    pub data: bool,
    /// `PRIVATE`: not listed in the import library.
    pub private: bool,
}

/// The export table of an image.
#[derive(Clone, Debug, Default)]
pub struct Exports {
    /// The exports, sorted by name.
    pub entries: Vec<Export>,
    /// The name recorded in the directory, and in import libraries.
    pub dll_name: Vec<u8>,
    /// The lowest ordinal in use.
    pub ordinal_base: u16,
}

impl Exports {
    /// Whether there is anything to write.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The highest ordinal in use.
    #[must_use]
    pub fn highest_ordinal(&self) -> u16 {
        self.entries
            .iter()
            .map(|export| export.ordinal)
            .max()
            .unwrap_or(self.ordinal_base)
    }

    /// Number of export address table slots: one per ordinal in range.
    #[must_use]
    pub fn address_slots(&self) -> u32 {
        if self.entries.is_empty() {
            return 0;
        }
        u32::from(self.highest_ordinal())
            .saturating_sub(u32::from(self.ordinal_base))
            .saturating_add(1)
    }

    /// The exports that have a name in the name pointer table.
    pub fn named(&self) -> impl Iterator<Item = &Export> {
        self.entries.iter().filter(|export| !export.noname)
    }

    /// Size of the whole `.edata` section.
    #[must_use]
    pub fn size(&self) -> u32 {
        if self.entries.is_empty() {
            return 0;
        }
        let names = u32::try_from(self.named().count()).unwrap_or(0);
        let strings = self
            .named()
            .map(|export| string_size(&export.name))
            .chain(
                self.entries
                    .iter()
                    .filter_map(|export| export.forwarder.as_deref().map(string_size)),
            )
            .fold(string_size(&self.dll_name), u32::saturating_add);
        EXPORT_DIRECTORY_SIZE
            .saturating_add(self.address_slots().saturating_mul(4))
            .saturating_add(names.saturating_mul(4))
            .saturating_add(names.saturating_mul(2))
            .saturating_add(strings)
    }

    /// Renders the section contents, with `rva` as the section's address.
    ///
    /// Exports whose symbol is not defined are reported and written as 0.
    #[must_use]
    pub fn render(
        &self,
        addresses: &Addresses<'_, '_>,
        rva: u32,
        diagnostics: &dyn DiagnosticSink,
    ) -> Vec<u8> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        let slots = self.address_slots();
        let names = u32::try_from(self.named().count()).unwrap_or(0);
        let eat_rva = rva.saturating_add(EXPORT_DIRECTORY_SIZE);
        let name_table_rva = eat_rva.saturating_add(slots.saturating_mul(4));
        let ordinal_table_rva = name_table_rva.saturating_add(names.saturating_mul(4));
        let strings_rva = ordinal_table_rva.saturating_add(names.saturating_mul(2));

        let mut strings: Vec<u8> = Vec::new();
        let push_string = |strings: &mut Vec<u8>, text: &[u8]| -> u32 {
            let at = strings_rva.saturating_add(u32::try_from(strings.len()).unwrap_or(0));
            strings.extend_from_slice(text);
            strings.push(0);
            at
        };
        let dll_name_rva = push_string(&mut strings, &self.dll_name);

        // The export address table, indexed by ordinal.
        let mut eat = vec![0u32; slots as usize];
        for export in &self.entries {
            let index = usize::from(export.ordinal.wrapping_sub(self.ordinal_base));
            let value = match (&export.forwarder, &export.symbol) {
                (Some(forwarder), _) => push_string(&mut strings, forwarder),
                (None, Some(symbol)) => match addresses.by_name(symbol) {
                    Some(Value::Address { rva, .. }) => rva,
                    _ => {
                        diagnostics.emit(Diagnostic::error(format!(
                            "exported symbol is not defined: {}",
                            String::from_utf8_lossy(symbol)
                        )));
                        0
                    }
                },
                (None, None) => 0,
            };
            if let Some(slot) = eat.get_mut(index) {
                *slot = value;
            }
        }

        let mut name_pointers: Vec<u32> = Vec::with_capacity(names as usize);
        let mut name_ordinals: Vec<u16> = Vec::with_capacity(names as usize);
        for export in self.named() {
            name_pointers.push(push_string(&mut strings, &export.name));
            name_ordinals.push(export.ordinal.wrapping_sub(self.ordinal_base));
        }

        let mut out: Vec<u8> = Vec::with_capacity(self.size() as usize);
        out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
        out.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
        out.extend_from_slice(&0u16.to_le_bytes()); // MajorVersion
        out.extend_from_slice(&0u16.to_le_bytes()); // MinorVersion
        out.extend_from_slice(&dll_name_rva.to_le_bytes());
        out.extend_from_slice(&u32::from(self.ordinal_base).to_le_bytes());
        out.extend_from_slice(&slots.to_le_bytes());
        out.extend_from_slice(&names.to_le_bytes());
        out.extend_from_slice(&eat_rva.to_le_bytes());
        out.extend_from_slice(&name_table_rva.to_le_bytes());
        out.extend_from_slice(&ordinal_table_rva.to_le_bytes());
        for value in &eat {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in &name_pointers {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in &name_ordinals {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&strings);
        out
    }
}

/// Size of a NUL-terminated string.
fn string_size(text: &[u8]) -> u32 {
    u32::try_from(text.len()).unwrap_or(0).saturating_add(1)
}

/// Builds the export table from the requests and, when nothing is requested,
/// from every symbol the image defines.
#[must_use]
pub fn plan(
    requests: &[ExportRequest],
    symbols: &SymbolTable<'_>,
    files: &[CoffInput<'_>],
    resolution: &crate::symbols::Resolution<'_>,
    pe: &PeOptions,
    dll_name: &[u8],
) -> Exports {
    let mut entries: Vec<Export> = Vec::new();
    for request in requests {
        if request.private {
            continue;
        }
        entries.push(Export {
            name: maybe_kill_at(&request.name, pe.kill_at),
            symbol: request
                .forwarder
                .is_none()
                .then(|| request.symbol().to_vec()),
            forwarder: request.forwarder.clone(),
            ordinal: request.ordinal.unwrap_or(0),
            noname: request.noname,
            data: request.data,
            private: request.private,
        });
    }
    // GNU ld exports everything a DLL defines when nothing asked for a
    // specific set, and `--export-all-symbols` forces it.
    let export_all =
        !pe.exclude_all_symbols && (pe.export_all_symbols || (pe.dll && entries.is_empty()));
    if export_all {
        for id in symbols.ids() {
            let definition = symbols.definition(id);
            if definition.kind != DefinitionKind::Regular || !resolution.is_live(definition.file) {
                continue;
            }
            let Some(global) = files
                .get(definition.file.index())
                .and_then(CoffInput::object)
                .and_then(|parsed| parsed.globals.get(definition.index as usize))
            else {
                continue;
            };
            let name = symbols.name(id).bytes();
            if is_filtered(name) || pe.exclude_symbols.iter().any(|entry| entry == name) {
                continue;
            }
            if entries.iter().any(|export| export.name == name) {
                continue;
            }
            entries.push(Export {
                name: maybe_kill_at(name, pe.kill_at),
                symbol: Some(name.to_vec()),
                forwarder: None,
                ordinal: 0,
                noname: false,
                data: !matches!(global.kind, GlobalKind::Defined { .. })
                    || !is_code(files, definition),
                private: false,
            });
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.dedup_by(|a, b| a.name == b.name);
    assign_ordinals(&mut entries);
    let ordinal_base = entries
        .iter()
        .map(|export| export.ordinal)
        .min()
        .unwrap_or(1);
    Exports {
        entries,
        dll_name: dll_name.to_vec(),
        ordinal_base,
    }
}

/// Whether the definition lives in an executable section.
fn is_code(files: &[CoffInput<'_>], definition: crate::symbols::Definition) -> bool {
    let Some(parsed) = files
        .get(definition.file.index())
        .and_then(CoffInput::object)
    else {
        return false;
    };
    let Some(global) = parsed.globals.get(definition.index as usize) else {
        return false;
    };
    let GlobalKind::Defined { section, .. } = global.kind else {
        return false;
    };
    parsed
        .section(section)
        .is_some_and(|section| section.header.is_code())
}

/// Gives every export without an explicit ordinal the lowest free one.
fn assign_ordinals(entries: &mut [Export]) {
    let mut used: Vec<u16> = entries
        .iter()
        .filter(|export| export.ordinal != 0)
        .map(|export| export.ordinal)
        .collect();
    used.sort_unstable();
    let mut next = 1u16;
    for export in entries.iter_mut() {
        if export.ordinal != 0 {
            continue;
        }
        while used.binary_search(&next).is_ok() {
            next = next.saturating_add(1);
        }
        export.ordinal = next;
        next = next.saturating_add(1);
    }
}

/// `--kill-at`: drops the `@N` suffix a stdcall name carries.
fn maybe_kill_at(name: &[u8], kill_at: bool) -> Vec<u8> {
    if !kill_at {
        return name.to_vec();
    }
    let body = name.strip_prefix(b"@").map_or(name, |rest| rest);
    match body.iter().rposition(|&byte| byte == b'@') {
        Some(at) if body.get(at.saturating_add(1)..).is_some_and(is_digits) => {
            body.get(..at).unwrap_or(body).to_vec()
        }
        _ => name.to_vec(),
    }
}

fn is_digits(text: &[u8]) -> bool {
    !text.is_empty() && text.iter().all(u8::is_ascii_digit)
}

/// Whether `--export-all-symbols` skips a name.
///
/// The list follows GNU `ld`'s `autofilter_symbollist` and its prefix and
/// suffix filters: the C runtime's own entry points, the import machinery,
/// and the symbols the linker itself defines.
#[must_use]
pub fn is_filtered(name: &[u8]) -> bool {
    const EXACT: &[&[u8]] = &[
        b"DllMain",
        b"DllMainCRTStartup",
        b"DllEntryPoint",
        b"impure_ptr",
        b"_cygwin_dll_entry",
        b"_cygwin_crt0_common",
        b"_cygwin_noncygwin_dll_entry",
        b"__dso_handle",
        b"_pei386_runtime_relocator",
        b"mainCRTStartup",
        b"WinMainCRTStartup",
        b"_tls_used",
        b"_load_config_used",
    ];
    const PREFIXES: &[&[u8]] = &[
        b"__imp_",
        b"_imp_",
        b"__rtti_",
        b"__builtin_",
        b"_head_",
        b"__CTOR_LIST__",
        b"__DTOR_LIST__",
        b"___CTOR_LIST__",
        b"___DTOR_LIST__",
        b"___crt_x",
        b"__RUNTIME_PSEUDO_RELOC_LIST",
        b"___RUNTIME_PSEUDO_RELOC_LIST",
        b"__ImageBase",
        b"___ImageBase",
        b"__image_base__",
        b".",
    ];
    const SUFFIXES: &[&[u8]] = &[b"_iname", b"_NULL_THUNK_DATA"];
    EXACT.contains(&name)
        || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// The name a `.def` file or `--out-implib` records for the image: the
/// output file's name.
#[must_use]
pub fn default_dll_name(path: &std::path::Path) -> Vec<u8> {
    path.file_name()
        .map_or_else(Vec::new, |name| name.as_encoded_bytes().to_vec())
}

/// Looks a symbol up for an export, for callers that need the ID.
#[must_use]
pub fn lookup(symbols: &SymbolTable<'_>, name: &[u8]) -> Option<SymbolId> {
    symbols.lookup(&SymbolName::new(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export(name: &[u8], ordinal: u16) -> Export {
        Export {
            name: name.to_vec(),
            symbol: Some(name.to_vec()),
            forwarder: None,
            ordinal,
            noname: false,
            data: false,
            private: false,
        }
    }

    #[test]
    fn ordinals_fill_the_gaps_explicit_ones_leave() {
        let mut entries = vec![export(b"a", 0), export(b"b", 2), export(b"c", 0)];
        assign_ordinals(&mut entries);
        assert_eq!(entries[0].ordinal, 1);
        assert_eq!(entries[1].ordinal, 2);
        assert_eq!(entries[2].ordinal, 3);
    }

    #[test]
    fn size_matches_the_rendered_length() {
        let exports = Exports {
            entries: vec![export(b"alpha", 1), export(b"beta", 2)],
            dll_name: b"sample.dll".to_vec(),
            ordinal_base: 1,
        };
        // 40 + 2*4 (EAT) + 2*4 (names) + 2*2 (ordinals) + strings.
        let strings = b"sample.dll\0alpha\0beta\0".len() as u32;
        assert_eq!(exports.size(), 40 + 8 + 8 + 4 + strings);
        assert_eq!(exports.address_slots(), 2);
    }

    #[test]
    fn kill_at_drops_the_stdcall_suffix() {
        assert_eq!(maybe_kill_at(b"_foo@8", true), b"_foo");
        assert_eq!(maybe_kill_at(b"_foo@8", false), b"_foo@8");
        assert_eq!(maybe_kill_at(b"plain", true), b"plain");
        assert_eq!(maybe_kill_at(b"foo@bar", true), b"foo@bar");
    }

    #[test]
    fn runtime_symbols_are_filtered() {
        assert!(is_filtered(b"DllMainCRTStartup"));
        assert!(is_filtered(b"__imp_printf"));
        assert!(is_filtered(b"libkernel32_a_iname"));
        assert!(is_filtered(b".text"));
        assert!(!is_filtered(b"my_function"));
    }
}
