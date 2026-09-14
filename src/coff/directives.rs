//! `.drectve` directives, collected from the objects that take part in the
//! link.
//!
//! MinGW objects carry `-export:` (from `__declspec(dllexport)`) and
//! `-aligncomm:`; MSVC objects add `-defaultlib:`, `-include:` and
//! `-alternatename:`. Directives are read when an object is parsed, and
//! applied as the link is set up.
//!
//! `-defaultlib:` and `-include:` change the input set, so they are read in a
//! pre-pass over the objects named on the command line, before resolution
//! starts. A directive in an archive member extracted later cannot add
//! inputs; qld warns instead of silently ignoring it (see
//! `docs/compatibility.md`).

#![deny(clippy::arithmetic_side_effects)]

use crate::error::Result;

use super::inputs::CoffInput;
use super::read::{Directive, ExportSpec, Source, parse_directives};

/// One export a directive or a `.def` file asks for, with owned names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportRequest {
    /// The exported name.
    pub name: Vec<u8>,
    /// `name=internal`: the symbol the export refers to.
    pub internal: Option<Vec<u8>>,
    /// `name=dll.function`: a forwarder.
    pub forwarder: Option<Vec<u8>>,
    /// `@ordinal`.
    pub ordinal: Option<u16>,
    /// `NONAME`: export by ordinal only.
    pub noname: bool,
    /// `DATA`: no thunk in the import library.
    pub data: bool,
    /// `PRIVATE`: not listed in the import library.
    pub private: bool,
}

impl ExportRequest {
    /// Converts a parsed `-export:` or `.def` `EXPORTS` entry.
    #[must_use]
    pub fn from_spec(spec: &ExportSpec<'_>) -> Self {
        let forwarder = spec.forwarder().map(<[u8]>::to_vec);
        Self {
            name: spec.export_as.as_deref().unwrap_or(&spec.name).to_vec(),
            internal: if forwarder.is_some() {
                None
            } else {
                spec.internal_name.as_deref().map(<[u8]>::to_vec)
            },
            forwarder,
            ordinal: spec.ordinal,
            noname: spec.noname,
            data: spec.data,
            private: spec.private,
        }
    }

    /// The symbol the export refers to.
    #[must_use]
    pub fn symbol(&self) -> &[u8] {
        self.internal.as_deref().unwrap_or(&self.name)
    }
}

/// Everything the directives of a link ask for.
#[derive(Clone, Debug, Default)]
pub struct Directives {
    /// `-export:` requests, in the order they were seen.
    pub exports: Vec<ExportRequest>,
    /// `-include:` symbols, which become link roots.
    pub includes: Vec<Vec<u8>>,
    /// `-defaultlib:` libraries.
    pub default_libs: Vec<Vec<u8>>,
    /// `-nodefaultlib:` libraries, and whether a bare `-nodefaultlib` was
    /// seen.
    pub no_default_libs: Vec<Vec<u8>>,
    /// Whether a bare `-nodefaultlib` suppressed every default library.
    pub no_default_lib: bool,
    /// `-alternatename:alias=target`.
    pub alternate_names: Vec<(Vec<u8>, Vec<u8>)>,
    /// `-aligncomm:symbol,log2`.
    pub align_comm: Vec<(Vec<u8>, u32)>,
    /// `-exclude-symbols:` entries.
    pub exclude_symbols: Vec<Vec<u8>>,
    /// `-entry:`.
    pub entry: Option<Vec<u8>>,
    /// `-subsystem:`.
    pub subsystem: Option<Vec<u8>>,
    /// `-stack:reserve[,commit]`.
    pub stack: Option<(u64, Option<u64>)>,
    /// `-heap:reserve[,commit]`.
    pub heap: Option<(u64, Option<u64>)>,
    /// `-merge:from=to`, which qld does not honor yet.
    pub merges: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Directives {
    /// Reads the `.drectve` sections of a parsed input and adds what they
    /// ask for.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Malformed`](crate::Error::Malformed) for a directive
    /// with an invalid value.
    pub fn add_from(&mut self, input: &CoffInput<'_>) -> Result<()> {
        let Some(parsed) = input.object() else {
            return Ok(());
        };
        let source = match input.file {
            Some(file) => super::inputs::source_of(file),
            None => Source::new(std::path::Path::new("<internal>")),
        };
        for data in &parsed.directives {
            for directive in parse_directives(data, 0, source) {
                self.apply(&directive?);
            }
        }
        Ok(())
    }

    /// Applies one directive.
    pub fn apply(&mut self, directive: &Directive<'_>) {
        match directive {
            Directive::Export(spec) => {
                let request = ExportRequest::from_spec(spec);
                if !self.exports.contains(&request) {
                    self.exports.push(request);
                }
            }
            Directive::Include(name) | Directive::IncludeOptional(name) => {
                push_unique(&mut self.includes, name);
            }
            Directive::DefaultLib(name) => push_unique(&mut self.default_libs, name),
            Directive::NoDefaultLib(None) => self.no_default_lib = true,
            Directive::NoDefaultLib(Some(name)) => {
                push_unique(&mut self.no_default_libs, name);
            }
            Directive::AlternateName { alias, target } => {
                let pair = (alias.to_vec(), target.to_vec());
                if !self.alternate_names.contains(&pair) {
                    self.alternate_names.push(pair);
                }
            }
            Directive::AlignComm {
                symbol,
                alignment_log2,
            } => self.align_comm.push((symbol.to_vec(), *alignment_log2)),
            Directive::ExcludeSymbols(list) => {
                for name in super::read::directives::split_list(list) {
                    push_unique(&mut self.exclude_symbols, name);
                }
            }
            Directive::Entry(name) => self.entry = Some(name.to_vec()),
            Directive::Subsystem(name) => self.subsystem = Some(name.to_vec()),
            Directive::Stack { reserve, commit } => self.stack = Some((*reserve, *commit)),
            Directive::Heap { reserve, commit } => self.heap = Some((*reserve, *commit)),
            Directive::Merge { from, to } => self.merges.push((from.to_vec(), to.to_vec())),
            _ => {}
        }
    }

    /// The default libraries to add, after `-nodefaultlib` and
    /// `--no-default-lib` are applied.
    #[must_use]
    pub fn wanted_libraries(&self, no_default_lib: bool) -> Vec<Vec<u8>> {
        if no_default_lib || self.no_default_lib {
            return Vec::new();
        }
        self.default_libs
            .iter()
            .filter(|name| {
                !self
                    .no_default_libs
                    .iter()
                    .any(|excluded| equal_ignoring_case(excluded, name))
            })
            .cloned()
            .collect()
    }
}

fn push_unique(list: &mut Vec<Vec<u8>>, name: &[u8]) {
    if !list.iter().any(|existing| existing == name) {
        list.push(name.to_vec());
    }
}

/// Library names are compared without regard to case, as on Windows, and
/// with a `.lib` suffix ignored.
fn equal_ignoring_case(left: &[u8], right: &[u8]) -> bool {
    let trim = |name: &[u8]| {
        let lowered: Vec<u8> = name.to_ascii_lowercase();
        match lowered.strip_suffix(b".lib") {
            Some(base) => base.to_vec(),
            None => lowered,
        }
    };
    trim(left) == trim(right)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    fn directive(text: &[u8]) -> Directive<'_> {
        super::super::read::directives::parse_token(text).unwrap()
    }

    #[test]
    fn collects_what_the_linker_acts_on() {
        let mut directives = Directives::default();
        for text in [
            &b"-export:foo,data"[..],
            b"-export:foo,data",
            b"-include:bar",
            b"-defaultlib:LIBCMT",
            b"-defaultlib:oldnames",
            b"-nodefaultlib:oldnames.lib",
            b"-alternatename:a=b",
            b"-aligncomm:c,4",
            b"-entry:go",
        ] {
            directives.apply(&directive(text));
        }
        assert_eq!(directives.exports.len(), 1, "duplicates are folded");
        assert!(directives.exports[0].data);
        assert_eq!(directives.includes, [b"bar".to_vec()]);
        assert_eq!(
            directives.wanted_libraries(false),
            [b"LIBCMT".to_vec()],
            "-nodefaultlib:oldnames.lib removes oldnames"
        );
        assert!(directives.wanted_libraries(true).is_empty());
        assert_eq!(directives.alternate_names, [(b"a".to_vec(), b"b".to_vec())]);
        assert_eq!(directives.align_comm, [(b"c".to_vec(), 4)]);
        assert_eq!(directives.entry.as_deref(), Some(&b"go"[..]));
    }

    #[test]
    fn forwarders_and_renames() {
        let spec = ExportSpec {
            name: Cow::Borrowed(b"outer"),
            internal_name: Some(Cow::Borrowed(b"inner")),
            ..ExportSpec::default()
        };
        let request = ExportRequest::from_spec(&spec);
        assert_eq!(request.symbol(), b"inner");
        assert!(request.forwarder.is_none());

        let spec = ExportSpec {
            name: Cow::Borrowed(b"Sleep"),
            internal_name: Some(Cow::Borrowed(b"kernel32.Sleep")),
            ..ExportSpec::default()
        };
        let request = ExportRequest::from_spec(&spec);
        assert_eq!(request.forwarder.as_deref(), Some(&b"kernel32.Sleep"[..]));
    }
}
