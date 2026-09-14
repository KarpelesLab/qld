//! Export specifications shared by `.drectve` `-export:` directives and
//! `.def` `EXPORTS` entries.

use std::borrow::Cow;

/// One exported symbol, as written in a `.def` file or an `-export:`
/// directive.
///
/// Names are kept exactly as written: no underscore is added or removed for
/// i386 decoration. That is the linker's decision, since it depends on the
/// target and on whether the name came from a `.def` file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ExportSpec<'a> {
    /// The exported name (the first name written).
    pub name: Cow<'a, [u8]>,
    /// `name=internal`: the symbol the export refers to. When it contains a
    /// `.`, it is a forwarder to `dll.function` instead (see
    /// [`forwarder`](Self::forwarder)).
    pub internal_name: Option<Cow<'a, [u8]>>,
    /// `name == import`: the name the import library imports (`.def` only).
    pub import_name: Option<Cow<'a, [u8]>>,
    /// `EXPORTAS name`: the name to export under, recorded in import
    /// libraries with `IMPORT_NAME_EXPORTAS`.
    pub export_as: Option<Cow<'a, [u8]>>,
    /// `@ordinal`.
    pub ordinal: Option<u16>,
    /// `NONAME`: export by ordinal only.
    pub noname: bool,
    /// `DATA`: a data export (no thunk in import libraries).
    pub data: bool,
    /// `PRIVATE`: not listed in the import library.
    pub private: bool,
    /// `CONSTANT`: an obsolete constant export.
    pub constant: bool,
}

impl ExportSpec<'_> {
    /// For `name=dll.function`, the forwarder target `dll.function`.
    #[must_use]
    pub fn forwarder(&self) -> Option<&[u8]> {
        self.internal_name
            .as_deref()
            .filter(|internal| internal.contains(&b'.'))
    }

    /// The symbol this export refers to: the internal name, or the exported
    /// name when there is none. `None` for forwarders.
    #[must_use]
    pub fn symbol(&self) -> Option<&[u8]> {
        if self.forwarder().is_some() {
            return None;
        }
        Some(self.internal_name.as_deref().unwrap_or(&self.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_and_forwarder() {
        let mut spec = ExportSpec {
            name: Cow::Borrowed(b"foo"),
            ..ExportSpec::default()
        };
        assert_eq!(spec.symbol(), Some(&b"foo"[..]));
        spec.internal_name = Some(Cow::Borrowed(b"bar"));
        assert_eq!(spec.symbol(), Some(&b"bar"[..]));
        assert_eq!(spec.forwarder(), None);
        spec.internal_name = Some(Cow::Borrowed(b"kernel32.Sleep"));
        assert_eq!(spec.symbol(), None);
        assert_eq!(spec.forwarder(), Some(&b"kernel32.Sleep"[..]));
    }
}
