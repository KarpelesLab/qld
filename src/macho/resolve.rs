//! Mach-O symbol precedence and resolution diagnostics.
//!
//! Precedence follows lld's Mach-O port: a regular definition beats a weak
//! one, which beats a tentative (common) definition, which beats a dylib
//! export, which beats a lazy archive member. Between two common symbols
//! the larger wins; other ties go to the earlier input position. Only two
//! regular definitions from different files are duplicates.

#![deny(clippy::arithmetic_side_effects)]

use core::cmp::Ordering;
use std::borrow::Cow;

use crate::diag::{Diagnostic, DiagnosticSink};
use crate::symbols::{Definition, DefinitionKind, DuplicateSymbol, Resolver};

use super::inputs::MachInput;

/// Mach-O symbol precedence.
#[derive(Clone, Copy, Debug, Default)]
pub struct MachRules;

fn rank(kind: DefinitionKind) -> u8 {
    match kind {
        DefinitionKind::Undefined => 0,
        DefinitionKind::Lazy => 1,
        DefinitionKind::Shared => 2,
        DefinitionKind::Common => 3,
        DefinitionKind::Weak => 4,
        DefinitionKind::Regular => 5,
    }
}

impl Resolver for MachRules {
    fn compare(&self, a: &Definition, b: &Definition) -> Ordering {
        match rank(a.kind).cmp(&rank(b.kind)) {
            Ordering::Equal if a.kind == DefinitionKind::Common => a.aux.cmp(&b.aux),
            other => other,
        }
    }

    fn is_duplicate(&self, winner: &Definition, other: &Definition) -> bool {
        winner.kind == DefinitionKind::Regular
            && other.kind == DefinitionKind::Regular
            && !winner.same_origin(other)
    }
}

/// A symbol name for messages: demangled C++ names lose the extra leading
/// underscore Mach-O adds.
#[must_use]
pub fn display_name(name: &[u8], demangle: bool) -> Cow<'_, str> {
    match name.strip_prefix(b"_") {
        Some(rest) if demangle && rest.starts_with(b"_Z") => {
            crate::hints::display_symbol(rest, true)
        }
        _ => String::from_utf8_lossy(name),
    }
}

/// Reports duplicate definitions; returns the number of errors.
#[must_use]
pub fn report_duplicates(
    duplicates: &[DuplicateSymbol<'_>],
    files: &[MachInput<'_>],
    demangle: bool,
    diagnostics: &dyn DiagnosticSink,
) -> usize {
    let mut errors = 0usize;
    for duplicate in duplicates {
        let name = display_name(duplicate.name.bytes(), demangle);
        let mut diagnostic = Diagnostic::error(format!("duplicate symbol: {name}"));
        let winner = files
            .get(duplicate.winner.file.index())
            .map_or_else(|| "<unknown>".to_string(), MachInput::display);
        diagnostic = diagnostic.detail(format!("defined in {winner}"));
        for other in &duplicate.others {
            let file = files
                .get(other.file.index())
                .map_or_else(|| "<unknown>".to_string(), MachInput::display);
            diagnostic = diagnostic.detail(format!("defined in {file}"));
        }
        diagnostics.emit(diagnostic);
        errors = errors.saturating_add(1);
    }
    errors
}
