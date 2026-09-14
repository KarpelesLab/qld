//! Module-definition (`.def`) files.
//!
//! The grammar accepted is the union of what GNU `dlltool`/ld
//! (`deffilep.y`) and LLVM (`COFFModuleDefinition.cpp`, used by lld and
//! `llvm-dlltool`) accept in their MinGW modes:
//!
//! ```text
//! LIBRARY [name] [BASE=address]          ; or NAME for an executable
//! DESCRIPTION "text"
//! STACKSIZE reserve[,commit]
//! HEAPSIZE reserve[,commit]
//! VERSION major[.minor]
//! SECTIONS
//!     name [READ] [WRITE] [EXECUTE] [SHARED]
//! EXPORTS
//!     name[=internal] [@ordinal [NONAME]] [DATA] [PRIVATE] [CONSTANT]
//!          [==importname] [EXPORTAS name]
//! IMPORTS
//!     [internal=]module.name|module.ordinal [==name]
//! ```
//!
//! Lexing follows LLVM: `;` starts a comment that runs to the end of the
//! line; `=`, `==` and `,` are tokens; a word runs to the next `=`, `,`,
//! `;` or whitespace (so `kernel32.Sleep` and `foo@4` are single words);
//! `"..."` (and, as in GNU, `'...'`) quotes a word. Keywords are
//! upper-case. Where the two implementations differ, qld accepts both:
//!
//! - `NONAME`, `DATA`, `PRIVATE` and `CONSTANT` are also accepted in lower
//!   case, as in GNU (lld reads a lower-case `data` as the next export's
//!   name);
//! - flags may be separated by commas, as in GNU;
//! - numbers may be hexadecimal with `0x`, as in GNU (lld accepts only
//!   decimal); a leading `0` does not make a number octal, unlike GNU;
//! - `DESCRIPTION`, `SECTIONS`, `IMPORTS` and `NONAME` without an ordinal
//!   are GNU extensions that lld rejects.
//!
//! Names are kept exactly as written; adding the i386 `_` prefix is the
//! linker's decision.

use std::borrow::Cow;

use super::export::ExportSpec;
use super::source::{Source, to_u64};
use crate::error::{Error, Result};

/// Whether the file declared a DLL or an executable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModuleKind {
    /// `LIBRARY`.
    Library,
    /// `NAME`.
    Executable,
}

/// A `SECTIONS` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DefSection<'a> {
    /// Section name.
    pub name: &'a [u8],
    /// `READ`.
    pub read: bool,
    /// `WRITE`.
    pub write: bool,
    /// `EXECUTE`.
    pub execute: bool,
    /// `SHARED`.
    pub shared: bool,
}

/// An `IMPORTS` entry (GNU only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DefImport<'a> {
    /// `internal=`: the local name, if different.
    pub internal_name: Option<&'a [u8]>,
    /// The module (DLL name without extension, or with it).
    pub module: &'a [u8],
    /// The imported name, when importing by name.
    pub name: Option<&'a [u8]>,
    /// The imported ordinal, when importing by ordinal.
    pub ordinal: Option<u16>,
    /// `==name`: the name the import is known by.
    pub import_name: Option<&'a [u8]>,
}

/// A parsed module-definition file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleDefinition<'a> {
    /// `LIBRARY` or `NAME`, whichever came last.
    pub kind: Option<ModuleKind>,
    /// The name after `LIBRARY` or `NAME`.
    pub name: Option<&'a [u8]>,
    /// `BASE=`.
    pub image_base: Option<u64>,
    /// `DESCRIPTION`.
    pub description: Option<&'a [u8]>,
    /// `STACKSIZE` reserve.
    pub stack_reserve: Option<u64>,
    /// `STACKSIZE` commit.
    pub stack_commit: Option<u64>,
    /// `HEAPSIZE` reserve.
    pub heap_reserve: Option<u64>,
    /// `HEAPSIZE` commit.
    pub heap_commit: Option<u64>,
    /// `VERSION` major.
    pub major_image_version: Option<u32>,
    /// `VERSION` minor.
    pub minor_image_version: Option<u32>,
    /// `EXPORTS` entries, in file order.
    pub exports: Vec<ExportSpec<'a>>,
    /// `SECTIONS` entries, in file order.
    pub sections: Vec<DefSection<'a>>,
    /// `IMPORTS` entries, in file order.
    pub imports: Vec<DefImport<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kw {
    Base,
    Code,
    Constant,
    Data,
    Description,
    Exports,
    ExportAs,
    Heapsize,
    Imports,
    Library,
    Name,
    Noname,
    Private,
    Sections,
    Stacksize,
    Version,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok<'a> {
    Eof,
    Ident(&'a [u8]),
    Comma,
    Equal,
    EqualEqual,
    Kw(Kw, &'a [u8]),
}

fn keyword(word: &[u8]) -> Option<Kw> {
    Some(match word {
        b"BASE" => Kw::Base,
        b"CODE" => Kw::Code,
        b"CONSTANT" | b"constant" => Kw::Constant,
        b"DATA" | b"data" => Kw::Data,
        b"DESCRIPTION" => Kw::Description,
        b"EXPORTS" => Kw::Exports,
        b"EXPORTAS" => Kw::ExportAs,
        b"HEAPSIZE" => Kw::Heapsize,
        b"IMPORTS" => Kw::Imports,
        b"LIBRARY" => Kw::Library,
        b"NAME" => Kw::Name,
        b"NONAME" | b"noname" => Kw::Noname,
        b"PRIVATE" | b"private" => Kw::Private,
        b"SECTIONS" => Kw::Sections,
        b"STACKSIZE" => Kw::Stacksize,
        b"VERSION" => Kw::Version,
        _ => return None,
    })
}

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    /// Start of the last token read.
    start: usize,
    /// A token pushed back by `unget`, with its start.
    pending: Option<(Tok<'a>, usize)>,
    last: Option<(Tok<'a>, usize)>,
    source: Source<'a>,
}

impl<'a> Parser<'a> {
    fn lex(&mut self) -> Result<Tok<'a>> {
        loop {
            let rest = self.data.get(self.pos..).unwrap_or_default();
            let skip = rest
                .iter()
                .position(|c| !c.is_ascii_whitespace() && *c != 0x0b)
                .unwrap_or(rest.len());
            self.pos = self.pos.saturating_add(skip);
            self.start = self.pos;
            let Some(&c) = self.data.get(self.pos) else {
                return Ok(Tok::Eof);
            };
            let rest = self.data.get(self.pos..).unwrap_or_default();
            match c {
                0 => return Ok(Tok::Eof),
                b';' => {
                    let line = rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len());
                    self.pos = self.pos.saturating_add(line);
                }
                b'=' => {
                    if rest.starts_with(b"==") {
                        self.pos = self.pos.saturating_add(2);
                        return Ok(Tok::EqualEqual);
                    }
                    self.pos = self.pos.saturating_add(1);
                    return Ok(Tok::Equal);
                }
                b',' => {
                    self.pos = self.pos.saturating_add(1);
                    return Ok(Tok::Comma);
                }
                b'"' | b'\'' => {
                    let body = rest.get(1..).unwrap_or_default();
                    let len = body
                        .iter()
                        .position(|&q| q == c)
                        .ok_or_else(|| self.error("unterminated quoted name"))?;
                    self.pos = self.pos.saturating_add(len).saturating_add(2);
                    return Ok(Tok::Ident(body.get(..len).unwrap_or_default()));
                }
                _ => {
                    let len = rest
                        .iter()
                        .position(|&c| {
                            matches!(c, b'=' | b',' | b';' | b'\r' | b'\n' | b' ' | b'\t' | 0x0b)
                        })
                        .unwrap_or(rest.len());
                    let word = rest.get(..len).unwrap_or_default();
                    self.pos = self.pos.saturating_add(len);
                    return Ok(match keyword(word) {
                        Some(kw) => Tok::Kw(kw, word),
                        None => Tok::Ident(word),
                    });
                }
            }
        }
    }

    fn read(&mut self) -> Result<Tok<'a>> {
        let tok = match self.pending.take() {
            Some((tok, start)) => {
                self.start = start;
                tok
            }
            None => self.lex()?,
        };
        self.last = Some((tok, self.start));
        Ok(tok)
    }

    fn unget(&mut self) {
        self.pending = self.last.take();
    }

    #[cold]
    fn error(&self, what: &str) -> Error {
        let before = self.data.get(..self.start).unwrap_or_default();
        let line = before
            .iter()
            .filter(|&&c| c == b'\n')
            .count()
            .saturating_add(1);
        self.source.malformed(
            to_u64(self.start),
            format!("module-definition file (line {line}: {what})"),
        )
    }

    fn identifier(&mut self, what: &str) -> Result<&'a [u8]> {
        match self.read()? {
            Tok::Ident(word) => Ok(word),
            _ => Err(self.error(what)),
        }
    }

    fn number(&mut self) -> Result<u64> {
        let word = self.identifier("expected a number")?;
        parse_def_number(word).ok_or_else(|| self.error("invalid number"))
    }

    fn parse(mut self) -> Result<ModuleDefinition<'a>> {
        let mut def = ModuleDefinition::default();
        loop {
            match self.read()? {
                Tok::Eof => return Ok(def),
                Tok::Kw(Kw::Exports, _) => loop {
                    match self.read()? {
                        Tok::Ident(name) => {
                            let export = self.export(name)?;
                            def.exports.push(export);
                        }
                        _ => {
                            self.unget();
                            break;
                        }
                    }
                },
                Tok::Kw(kind @ (Kw::Library | Kw::Name), _) => {
                    def.kind = Some(if kind == Kw::Library {
                        ModuleKind::Library
                    } else {
                        ModuleKind::Executable
                    });
                    def.name = match self.read()? {
                        Tok::Ident(name) => Some(name),
                        _ => {
                            self.unget();
                            None
                        }
                    };
                    if let Tok::Kw(Kw::Base, _) = self.read()? {
                        if self.read()? != Tok::Equal {
                            return Err(self.error("expected `=` after BASE"));
                        }
                        def.image_base = Some(self.number()?);
                    } else {
                        self.unget();
                    }
                }
                Tok::Kw(Kw::Description, _) => {
                    def.description = Some(self.identifier("expected a description")?);
                }
                Tok::Kw(kw @ (Kw::Stacksize | Kw::Heapsize), _) => {
                    let reserve = self.number()?;
                    let commit = if self.read()? == Tok::Comma {
                        Some(self.number()?)
                    } else {
                        self.unget();
                        None
                    };
                    if kw == Kw::Stacksize {
                        def.stack_reserve = Some(reserve);
                        def.stack_commit = commit;
                    } else {
                        def.heap_reserve = Some(reserve);
                        def.heap_commit = commit;
                    }
                }
                Tok::Kw(Kw::Version, _) => {
                    let word = self.identifier("expected a version")?;
                    let (major, minor) = match word.iter().position(|&c| c == b'.') {
                        Some(dot) => (
                            word.get(..dot).unwrap_or_default(),
                            word.get(dot.saturating_add(1)..),
                        ),
                        None => (word, None),
                    };
                    let parse =
                        |part: &[u8]| parse_def_number(part).and_then(|v| u32::try_from(v).ok());
                    def.major_image_version =
                        Some(parse(major).ok_or_else(|| self.error("invalid version"))?);
                    def.minor_image_version = Some(match minor {
                        Some(minor) => parse(minor).ok_or_else(|| self.error("invalid version"))?,
                        None => 0,
                    });
                }
                Tok::Kw(Kw::Sections, _) => self.sections(&mut def)?,
                Tok::Kw(Kw::Imports, _) => self.imports(&mut def)?,
                Tok::Kw(kw @ (Kw::Code | Kw::Data), _) => {
                    // GNU `CODE attrs` / `DATA attrs`: attributes of the
                    // default code or data section.
                    let name: &'a [u8] = if kw == Kw::Code { b"CODE" } else { b"DATA" };
                    let section = self.section_attributes(name)?;
                    def.sections.push(section);
                }
                _ => return Err(self.error("unknown directive")),
            }
        }
    }

    fn export(&mut self, name: &'a [u8]) -> Result<ExportSpec<'a>> {
        let mut export = ExportSpec {
            name: Cow::Borrowed(name),
            ..ExportSpec::default()
        };
        if self.read()? == Tok::Equal {
            export.internal_name =
                Some(Cow::Borrowed(self.identifier("expected a name after `=`")?));
        } else {
            self.unget();
        }
        loop {
            match self.read()? {
                Tok::Ident(word) if word.first() == Some(&b'@') => {
                    let digits = word.get(1..).unwrap_or_default();
                    let ordinal = if digits.is_empty() {
                        // "foo @ 10"
                        let number = self.number()?;
                        Some(u16::try_from(number).map_err(|_| self.error("invalid ordinal"))?)
                    } else {
                        parse_def_number(digits).and_then(|v| u16::try_from(v).ok())
                    };
                    let Some(ordinal) = ordinal else {
                        // "foo \n @bar": the next export, fastcall-decorated.
                        self.unget();
                        return Ok(export);
                    };
                    export.ordinal = Some(ordinal);
                }
                Tok::Kw(Kw::Noname, _) => export.noname = true,
                Tok::Kw(Kw::Data, _) => export.data = true,
                Tok::Kw(Kw::Constant, _) => export.constant = true,
                Tok::Kw(Kw::Private, _) => export.private = true,
                Tok::Comma => {}
                Tok::EqualEqual => {
                    export.import_name = Some(Cow::Borrowed(
                        self.identifier("expected a name after `==`")?,
                    ));
                }
                Tok::Kw(Kw::ExportAs, _) => {
                    export.export_as = Some(Cow::Borrowed(
                        self.identifier("expected a name after EXPORTAS")?,
                    ));
                    return Ok(export);
                }
                _ => {
                    self.unget();
                    return Ok(export);
                }
            }
        }
    }

    fn section_attributes(&mut self, name: &'a [u8]) -> Result<DefSection<'a>> {
        let mut section = DefSection {
            name,
            read: false,
            write: false,
            execute: false,
            shared: false,
        };
        loop {
            match self.read()? {
                Tok::Ident(b"READ") => section.read = true,
                Tok::Ident(b"WRITE") => section.write = true,
                Tok::Ident(b"EXECUTE") => section.execute = true,
                Tok::Ident(b"SHARED") => section.shared = true,
                Tok::Comma => {}
                _ => {
                    self.unget();
                    return Ok(section);
                }
            }
        }
    }

    fn sections(&mut self, def: &mut ModuleDefinition<'a>) -> Result<()> {
        loop {
            match self.read()? {
                Tok::Ident(name) => {
                    let section = self.section_attributes(name)?;
                    def.sections.push(section);
                }
                _ => {
                    self.unget();
                    return Ok(());
                }
            }
        }
    }

    fn imports(&mut self, def: &mut ModuleDefinition<'a>) -> Result<()> {
        loop {
            let first = match self.read()? {
                Tok::Ident(word) => word,
                _ => {
                    self.unget();
                    return Ok(());
                }
            };
            let (internal_name, target) = if self.read()? == Tok::Equal {
                (Some(first), self.identifier("expected module.name")?)
            } else {
                self.unget();
                (None, first)
            };
            let dot = target
                .iter()
                .rposition(|&c| c == b'.')
                .ok_or_else(|| self.error("expected module.name"))?;
            let module = target.get(..dot).unwrap_or_default();
            let entry = target.get(dot.saturating_add(1)..).unwrap_or_default();
            let (name, ordinal) = if entry.first().is_some_and(u8::is_ascii_digit) {
                let ordinal = parse_def_number(entry)
                    .and_then(|v| u16::try_from(v).ok())
                    .ok_or_else(|| self.error("invalid ordinal"))?;
                (None, Some(ordinal))
            } else {
                (Some(entry), None)
            };
            let import_name = if self.read()? == Tok::EqualEqual {
                Some(self.identifier("expected a name after `==`")?)
            } else {
                self.unget();
                None
            };
            def.imports.push(DefImport {
                internal_name,
                module,
                name,
                ordinal,
                import_name,
            });
        }
    }
}

/// Parses a `.def` number: decimal, or hexadecimal with `0x`.
fn parse_def_number(word: &[u8]) -> Option<u64> {
    match word
        .strip_prefix(b"0x")
        .or_else(|| word.strip_prefix(b"0X"))
    {
        Some(hex) => super::directives::parse_radix(hex, 16),
        None => super::directives::parse_radix(word, 10),
    }
}

/// Parses a module-definition file.
///
/// # Errors
///
/// Returns `Error::Malformed` naming the line of the first syntax error.
pub fn parse_module_definition<'a>(
    data: &'a [u8],
    source: Source<'a>,
) -> Result<ModuleDefinition<'a>> {
    Parser {
        data,
        pos: 0,
        start: 0,
        pending: None,
        last: None,
        source,
    }
    .parse()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn parse(text: &'static str) -> Result<ModuleDefinition<'static>> {
        parse_module_definition(text.as_bytes(), Source::new(Path::new("x.def")))
    }

    #[test]
    fn full_file() {
        let def = parse(
            "; comment\n\
             LIBRARY \"foo.dll\" BASE=0x10000000\n\
             DESCRIPTION 'a library'\n\
             STACKSIZE 0x100000, 4096\n\
             HEAPSIZE 1048576\n\
             VERSION 1.2\n\
             EXPORTS\n\
             \x20 plain\n\
             \x20 ext=internal @3 NONAME\n\
             \x20 var DATA ; trailing\n\
             \x20 fwd = kernel32.Sleep\n\
             \x20 _fast@8 @ 7, PRIVATE\n\
             \x20 @fastcall@4\n\
             \x20 imp==realname CONSTANT\n\
             \x20 alias EXPORTAS real\n\
             \x20 lower data private\n\
             SECTIONS\n\
             \x20 .shared READ WRITE SHARED\n\
             IMPORTS\n\
             \x20 local=user32.MessageBoxA\n\
             \x20 gdi32.12\n",
        )
        .unwrap();
        assert_eq!(def.kind, Some(ModuleKind::Library));
        assert_eq!(def.name, Some(&b"foo.dll"[..]));
        assert_eq!(def.image_base, Some(0x1000_0000));
        assert_eq!(def.description, Some(&b"a library"[..]));
        assert_eq!(
            (def.stack_reserve, def.stack_commit),
            (Some(0x10_0000), Some(4096))
        );
        assert_eq!((def.heap_reserve, def.heap_commit), (Some(1_048_576), None));
        assert_eq!(
            (def.major_image_version, def.minor_image_version),
            (Some(1), Some(2))
        );
        let names: Vec<_> = def.exports.iter().map(|e| &*e.name).collect();
        assert_eq!(
            names,
            [
                &b"plain"[..],
                b"ext",
                b"var",
                b"fwd",
                b"_fast@8",
                b"@fastcall@4",
                b"imp",
                b"alias",
                b"lower"
            ]
        );
        let e = &def.exports[1];
        assert_eq!(e.internal_name.as_deref(), Some(&b"internal"[..]));
        assert_eq!(e.ordinal, Some(3));
        assert!(e.noname);
        assert!(def.exports[2].data);
        assert_eq!(def.exports[3].forwarder(), Some(&b"kernel32.Sleep"[..]));
        assert_eq!(def.exports[4].ordinal, Some(7));
        assert!(def.exports[4].private);
        assert_eq!(def.exports[5].ordinal, None);
        assert_eq!(
            def.exports[6].import_name.as_deref(),
            Some(&b"realname"[..])
        );
        assert!(def.exports[6].constant);
        assert_eq!(def.exports[7].export_as.as_deref(), Some(&b"real"[..]));
        assert!(def.exports[8].data && def.exports[8].private);
        assert_eq!(
            def.sections,
            [DefSection {
                name: b".shared",
                read: true,
                write: true,
                execute: false,
                shared: true
            }]
        );
        assert_eq!(def.imports.len(), 2);
        assert_eq!(def.imports[0].internal_name, Some(&b"local"[..]));
        assert_eq!(def.imports[0].module, b"user32");
        assert_eq!(def.imports[0].name, Some(&b"MessageBoxA"[..]));
        assert_eq!(def.imports[1].ordinal, Some(12));
    }

    #[test]
    fn name_and_errors() {
        let def = parse("NAME app.exe\nEXPORTS\n").unwrap();
        assert_eq!(def.kind, Some(ModuleKind::Executable));
        let def = parse("LIBRARY\nEXPORTS foo").unwrap();
        assert_eq!(def.name, None);
        assert_eq!(def.exports.len(), 1);
        assert!(parse("").unwrap().exports.is_empty());
        for bad in [
            "BOGUS",
            "LIBRARY foo BASE 5",
            "LIBRARY foo BASE=zz",
            "EXPORTS foo=",
            "EXPORTS foo @ x",
            "EXPORTS foo EXPORTAS",
            "EXPORTS \"foo",
            "VERSION a.b",
            "STACKSIZE",
            "IMPORTS nodot",
        ] {
            let error = parse(bad).unwrap_err().to_string();
            assert!(error.contains("line 1"), "{bad}: {error}");
        }
        let error = parse("EXPORTS\n  foo\n  BASE\n").unwrap_err().to_string();
        assert!(error.contains("line 3"), "{error}");
    }
}
