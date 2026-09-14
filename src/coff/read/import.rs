//! Import libraries: short import objects (`IMPORT_OBJECT_HEADER`, as
//! written by `link.exe`, `llvm-dlltool` and `llvm-lib`) and the long
//! import members GNU `dlltool` writes (regular COFF objects made of
//! `.idata$N` sections).

use hashbrown::HashMap;

use super::consts::{
    IMPORT_CODE, IMPORT_CONST, IMPORT_DATA, IMPORT_NAME_EXPORTAS, IMPORT_NAME_NOPREFIX,
    IMPORT_NAME_UNDECORATE, IMPORT_ORDINAL, machine_architecture,
};
use super::object::CoffObject;
use super::source::{Source, c_string, subslice, u16_at, u32_at, u64_at};
use super::symbol::SectionNumber;
use crate::error::Result;
use crate::target::Architecture;

/// Size of `IMPORT_OBJECT_HEADER`.
pub const IMPORT_OBJECT_HEADER_SIZE: usize = 20;

/// Prefix of the import address table symbol an import defines.
pub const IMP_PREFIX: &[u8] = b"__imp_";

/// What an import refers to in the DLL.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImportName<'a> {
    /// Import by ordinal.
    Ordinal(u16),
    /// Import by name, with a hint (an index into the DLL's export name
    /// table where the loader starts looking).
    Name {
        /// The hint.
        hint: u16,
        /// The name as exported by the DLL.
        name: &'a [u8],
    },
}

/// A parsed short import object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortImport<'a> {
    /// `Version` (0).
    pub version: u16,
    /// `Machine` (`IMAGE_FILE_MACHINE_*`).
    pub machine: u16,
    /// `TimeDateStamp`.
    pub time_date_stamp: u32,
    /// `SizeOfData`: bytes of strings after the header.
    pub size_of_data: u32,
    /// `OrdinalOrHint`.
    pub ordinal_or_hint: u16,
    /// Import type (`IMPORT_CODE`, `IMPORT_DATA`, `IMPORT_CONST`).
    pub import_type: u8,
    /// Name type (`IMPORT_ORDINAL`, `IMPORT_NAME`, …).
    pub name_type: u8,
    /// The public symbol name (without `__imp_`).
    pub symbol_name: &'a [u8],
    /// The DLL name.
    pub dll_name: &'a [u8],
    /// For `IMPORT_NAME_EXPORTAS`, the name stored after the DLL name.
    pub export_as: Option<&'a [u8]>,
}

impl<'a> ShortImport<'a> {
    /// Parses a short import object.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the signature is wrong, the data is
    /// truncated, or a string is not NUL-terminated.
    pub fn parse(data: &'a [u8], source: Source<'a>) -> Result<Self> {
        let field16 = |offset| {
            u16_at(data, offset)
                .ok_or_else(|| source.malformed(0, "import object header (truncated)"))
        };
        let field32 = |offset| {
            u32_at(data, offset)
                .ok_or_else(|| source.malformed(0, "import object header (truncated)"))
        };
        if field16(0)? != 0 || field16(2)? != 0xffff {
            return Err(source.malformed(0, "import object signature"));
        }
        let version = field16(4)?;
        if version != 0 {
            return Err(source.malformed(4, "import object version"));
        }
        let size_of_data = field32(12)?;
        let strings = subslice(data, 20, u64::from(size_of_data))
            .ok_or_else(|| source.malformed(12, "import object data size (out of bounds)"))?;
        let type_info = field16(18)?;
        let import_type = u8::try_from(type_info & 0x3).unwrap_or(0);
        let name_type = u8::try_from((type_info >> 2) & 0x7).unwrap_or(0);
        let symbol_name = c_string(strings)
            .ok_or_else(|| source.malformed(20, "import object symbol name (unterminated)"))?;
        let rest = strings
            .get(symbol_name.len().saturating_add(1)..)
            .unwrap_or_default();
        let dll_name = c_string(rest)
            .ok_or_else(|| source.malformed(20, "import object DLL name (unterminated)"))?;
        let export_as = if name_type == IMPORT_NAME_EXPORTAS {
            let rest = rest
                .get(dll_name.len().saturating_add(1)..)
                .unwrap_or_default();
            Some(
                c_string(rest)
                    .ok_or_else(|| source.malformed(20, "import object EXPORTAS name"))?,
            )
        } else {
            None
        };
        if symbol_name.is_empty() {
            return Err(source.malformed(20, "import object symbol name (empty)"));
        }
        Ok(Self {
            version,
            machine: field16(6)?,
            time_date_stamp: field32(8)?,
            size_of_data,
            ordinal_or_hint: field16(16)?,
            import_type,
            name_type,
            symbol_name,
            dll_name,
            export_as,
        })
    }

    /// The target architecture, if qld knows the machine.
    #[must_use]
    pub fn architecture(&self) -> Option<Architecture> {
        machine_architecture(self.machine)
    }

    /// Whether this imports code: it defines a thunk symbol named
    /// [`symbol_name`](Self::symbol_name) as well as `__imp_<symbol_name>`.
    #[must_use]
    pub fn is_code(&self) -> bool {
        self.import_type == IMPORT_CODE
    }

    /// Whether this imports data (`IMPORT_DATA`).
    #[must_use]
    pub fn is_data(&self) -> bool {
        self.import_type == IMPORT_DATA
    }

    /// Whether this imports a constant (`IMPORT_CONST`).
    #[must_use]
    pub fn is_const(&self) -> bool {
        self.import_type == IMPORT_CONST
    }

    /// The name or ordinal to import, derived from the name type as LLVM
    /// and lld do:
    ///
    /// - `IMPORT_ORDINAL`: the ordinal in `OrdinalOrHint`;
    /// - `IMPORT_NAME`: the symbol name;
    /// - `IMPORT_NAME_NOPREFIX`: the symbol name without one leading `?`,
    ///   `@` or `_`;
    /// - `IMPORT_NAME_UNDECORATE`: the same, cut at the first `@`;
    /// - `IMPORT_NAME_EXPORTAS`: the extra name after the DLL name.
    ///
    /// Unknown name types are treated as `IMPORT_NAME`.
    #[must_use]
    pub fn import_name(&self) -> ImportName<'a> {
        let hint = self.ordinal_or_hint;
        let strip_prefix = |name: &'a [u8]| match name.first() {
            Some(b'?' | b'@' | b'_') => name.get(1..).unwrap_or_default(),
            _ => name,
        };
        let name = match self.name_type {
            IMPORT_ORDINAL => return ImportName::Ordinal(hint),
            IMPORT_NAME_NOPREFIX => strip_prefix(self.symbol_name),
            IMPORT_NAME_UNDECORATE => {
                let name = strip_prefix(self.symbol_name);
                match name.iter().position(|&c| c == b'@') {
                    Some(at) => name.get(..at).unwrap_or(name),
                    None => name,
                }
            }
            IMPORT_NAME_EXPORTAS => self.export_as.unwrap_or(self.symbol_name),
            _ => self.symbol_name,
        };
        ImportName::Name { hint, name }
    }
}

// ---------------------------------------------------------------------------
// Long (dlltool) import libraries
// ---------------------------------------------------------------------------

/// What a member of a GNU `dlltool` import library provides.
///
/// Such a library has three kinds of members, all plain COFF objects:
///
/// - a **head** (`<lib>h.o`): an `.idata$2` import directory entry,
///   defining `_head_<lib>` (i386: `__head_<lib>`) and referring to the
///   DLL name symbol `<lib>_iname`;
/// - a **tail** (`<lib>t.o`): the null `.idata$4`/`.idata$5` terminators and
///   the DLL name in `.idata$7`, defining `<lib>_iname`;
/// - one **symbol** member per import (`<lib>s<N>.o`): the lookup and
///   address table entries (`.idata$4`, `.idata$5`, defining `__imp_<sym>`),
///   the hint and name (`.idata$6`), a reference to the head symbol from
///   `.idata$7`, and, for code, a `jmp *__imp_<sym>` thunk in `.text`
///   defining `<sym>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LongImportMember<'a> {
    /// The head member.
    Head {
        /// The head symbol it defines.
        head_symbol: &'a [u8],
        /// The DLL name symbol its import directory entry refers to.
        iname_symbol: Option<&'a [u8]>,
    },
    /// The tail member.
    Tail {
        /// The DLL name symbol it defines.
        iname_symbol: &'a [u8],
        /// The DLL name.
        dll_name: &'a [u8],
    },
    /// A member importing one symbol.
    Symbol(LongImportSymbol<'a>),
}

/// One import from a GNU `dlltool` import library member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LongImportSymbol<'a> {
    /// The head symbol the member refers to, which leads to the DLL name
    /// (see [`LongImportDlls`]).
    pub head_symbol: &'a [u8],
    /// The `__imp_` symbol defined in `.idata$5`.
    pub imp_symbol: &'a [u8],
    /// The thunk symbol defined in `.text`, for code imports.
    pub thunk_symbol: Option<&'a [u8]>,
    /// The name or ordinal imported.
    pub import: ImportName<'a>,
}

impl<'a> LongImportSymbol<'a> {
    /// The public symbol name: the thunk name, or the `__imp_` name without
    /// its prefix for data imports.
    #[must_use]
    pub fn symbol_name(&self) -> &'a [u8] {
        self.thunk_symbol.unwrap_or_else(|| {
            self.imp_symbol
                .strip_prefix(IMP_PREFIX)
                .unwrap_or(self.imp_symbol)
        })
    }
}

/// Section numbers of the `.idata$N` sections of a member.
#[derive(Clone, Copy, Debug, Default)]
struct IdataSections {
    idata2: Option<u32>,
    idata5: Option<u32>,
    idata6: Option<u32>,
    idata7: Option<u32>,
}

/// Recognizes a GNU `dlltool` long import library member.
///
/// Returns `Ok(None)` for objects that are not import members (they have
/// none of the `.idata$2`, `.idata$5` or `.idata$7` shapes described in
/// [`LongImportMember`]).
///
/// # Errors
///
/// Returns `Error::Malformed` if a section, symbol or relocation the
/// classification needs cannot be decoded.
pub fn classify_long_import<'a>(object: &CoffObject<'a>) -> Result<Option<LongImportMember<'a>>> {
    let mut idata = IdataSections::default();
    let mut code_sections = 0u64;
    for section in object.sections() {
        let section = section?;
        let slot = match section.name {
            b".idata$2" => &mut idata.idata2,
            b".idata$5" => &mut idata.idata5,
            b".idata$6" => &mut idata.idata6,
            b".idata$7" => &mut idata.idata7,
            _ => {
                if section.header.is_code() && section.number < 64 {
                    code_sections |= 1u64 << section.number;
                }
                continue;
            }
        };
        slot.get_or_insert(section.number);
    }
    if idata.idata2.is_none() && idata.idata5.is_none() && idata.idata7.is_none() {
        return Ok(None);
    }

    let defined_in = |number: Option<u32>, filter: &dyn Fn(&[u8]) -> bool| -> Result<_> {
        let Some(number) = number else {
            return Ok(None);
        };
        for symbol in object.symbols().iter() {
            let symbol = symbol?;
            if symbol.is_external()
                && symbol.section() == SectionNumber::Section(number)
                && filter(symbol.name)
            {
                return Ok(Some(symbol.name));
            }
        }
        Ok(None)
    };

    if let Some(idata2) = idata.idata2 {
        let Some(head_symbol) = defined_in(Some(idata2), &|_| true)? else {
            return Ok(None);
        };
        // The DLL name RVA is the fourth field (offset 12) of the entry.
        let iname_symbol = relocation_target(object, idata2, |offset| offset == 12)?;
        return Ok(Some(LongImportMember::Head {
            head_symbol,
            iname_symbol,
        }));
    }

    if let Some(iname_symbol) = defined_in(idata.idata7, &|_| true)? {
        let Some(number) = idata.idata7 else {
            return Ok(None);
        };
        let section = object.section(number)?;
        let data = object.section_data(&section.header)?;
        let dll_name = c_string(data).unwrap_or(data);
        return Ok(Some(LongImportMember::Tail {
            iname_symbol,
            dll_name,
        }));
    }

    let Some(imp_symbol) = defined_in(idata.idata5, &|name| name.starts_with(IMP_PREFIX))? else {
        return Ok(None);
    };
    let Some(head_symbol) = (match idata.idata7 {
        Some(number) => relocation_target(object, number, |_| true)?,
        None => None,
    }) else {
        return Ok(None);
    };
    let import = match idata.idata6 {
        Some(number) => {
            let section = object.section(number)?;
            let data = object.section_data(&section.header)?;
            let hint = u16_at(data, 0).unwrap_or(0);
            let tail = data.get(2..).unwrap_or_default();
            ImportName::Name {
                hint,
                name: c_string(tail).unwrap_or(tail),
            }
        }
        None => {
            let Some(number) = idata.idata5 else {
                return Ok(None);
            };
            let section = object.section(number)?;
            let data = object.section_data(&section.header)?;
            let ordinal = match data.len() {
                8 => u64_at(data, 0)
                    .filter(|entry| entry >> 63 != 0)
                    .map(|entry| entry & 0xffff),
                4 => u32_at(data, 0)
                    .filter(|entry| entry >> 31 != 0)
                    .map(|entry| u64::from(entry & 0xffff)),
                _ => None,
            };
            let Some(ordinal) = ordinal.and_then(|o| u16::try_from(o).ok()) else {
                return Ok(None);
            };
            ImportName::Ordinal(ordinal)
        }
    };
    let thunk_symbol = defined_in_code(object, code_sections)?;
    Ok(Some(LongImportMember::Symbol(LongImportSymbol {
        head_symbol,
        imp_symbol,
        thunk_symbol,
        import,
    })))
}

/// The first external symbol defined in one of the code sections in the
/// `code_sections` bit set.
fn defined_in_code<'a>(object: &CoffObject<'a>, code_sections: u64) -> Result<Option<&'a [u8]>> {
    if code_sections == 0 {
        return Ok(None);
    }
    for symbol in object.symbols().iter() {
        let symbol = symbol?;
        if let SectionNumber::Section(number) = symbol.section()
            && symbol.is_external()
            && number < 64
            && code_sections & (1u64 << number) != 0
        {
            return Ok(Some(symbol.name));
        }
    }
    Ok(None)
}

/// The name of the undefined external symbol targeted by the first
/// relocation of section `number` whose offset satisfies `at`.
fn relocation_target<'a>(
    object: &CoffObject<'a>,
    number: u32,
    at: impl Fn(u32) -> bool,
) -> Result<Option<&'a [u8]>> {
    let section = object.section(number)?;
    for reloc in object.relocations(&section.header)?.iter() {
        if !at(reloc.virtual_address) {
            continue;
        }
        let symbol = object.symbol(reloc.symbol_table_index)?;
        if symbol.is_undefined() {
            return Ok(Some(symbol.name));
        }
    }
    Ok(None)
}

/// Resolves the DLL of each symbol member of a GNU `dlltool` import library,
/// from its head and tail members.
///
/// Members arrive in any order; call [`add`](Self::add) for every
/// classified member, then [`dll_name`](Self::dll_name) for each symbol
/// member's head symbol.
#[derive(Clone, Debug, Default)]
pub struct LongImportDlls<'a> {
    heads: HashMap<&'a [u8], &'a [u8], foldhash::fast::FixedState>,
    tails: HashMap<&'a [u8], &'a [u8], foldhash::fast::FixedState>,
}

impl<'a> LongImportDlls<'a> {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a head or tail member; symbol members are ignored.
    pub fn add(&mut self, member: &LongImportMember<'a>) {
        match *member {
            LongImportMember::Head {
                head_symbol,
                iname_symbol: Some(iname),
            } => {
                self.heads.insert(head_symbol, iname);
            }
            LongImportMember::Tail {
                iname_symbol,
                dll_name,
            } => {
                self.tails.insert(iname_symbol, dll_name);
            }
            LongImportMember::Head { .. } | LongImportMember::Symbol(_) => {}
        }
    }

    /// The DLL name for a symbol member's head symbol, if both the head and
    /// the tail member were added.
    #[must_use]
    pub fn dll_name(&self, head_symbol: &[u8]) -> Option<&'a [u8]> {
        let iname = self.heads.get(head_symbol)?;
        self.tails.get(iname).copied()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn short(type_info: u16, strings: &[u8]) -> Vec<u8> {
        let mut data = vec![0, 0, 0xff, 0xff, 0, 0, 0x64, 0x86, 0, 0, 0, 0];
        data.extend(u32::try_from(strings.len()).unwrap().to_le_bytes());
        data.extend(7u16.to_le_bytes());
        data.extend(type_info.to_le_bytes());
        data.extend(strings);
        data
    }

    #[test]
    fn short_imports() {
        let source = Source::new(Path::new("x.lib"));
        let data = short(1 << 2, b"GetLastError\0KERNEL32.dll\0");
        let import = ShortImport::parse(&data, source).unwrap();
        assert_eq!(import.symbol_name, b"GetLastError");
        assert_eq!(import.dll_name, b"KERNEL32.dll");
        assert!(import.is_code());
        assert_eq!(
            import.import_name(),
            ImportName::Name {
                hint: 7,
                name: b"GetLastError"
            }
        );

        let data = short(1 | (3 << 2), b"_foo@4\0a.dll\0");
        let import = ShortImport::parse(&data, source).unwrap();
        assert!(import.is_data());
        assert_eq!(
            import.import_name(),
            ImportName::Name {
                hint: 7,
                name: b"foo"
            }
        );
        let data = short(2 << 2, b"?bar\0a.dll\0");
        let import = ShortImport::parse(&data, source).unwrap();
        assert_eq!(
            import.import_name(),
            ImportName::Name {
                hint: 7,
                name: b"bar"
            }
        );
        let data = short(0, b"baz\0a.dll\0");
        let import = ShortImport::parse(&data, source).unwrap();
        assert_eq!(import.import_name(), ImportName::Ordinal(7));
        let data = short(4 << 2, b"sym\0a.dll\0real\0");
        let import = ShortImport::parse(&data, source).unwrap();
        assert_eq!(
            import.import_name(),
            ImportName::Name {
                hint: 7,
                name: b"real"
            }
        );

        for bad in [
            &short(4 << 2, b"sym\0a.dll\0real")[..],
            &short(0, b"sym\0a.dll")[..],
            &short(0, b"\0a.dll\0")[..],
            &data[..data.len() - 1],
            &data[..10],
        ] {
            assert!(ShortImport::parse(bad, source).is_err());
        }
    }
}
