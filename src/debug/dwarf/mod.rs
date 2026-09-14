//! Source line lookup for diagnostics: turns a (section, offset) position
//! in a relocatable object into `file:line`.
//!
//! This runs only when a diagnostic is about to be printed, so it favors
//! simplicity over speed: [`LineLookup::parse`] loads the object's debug
//! sections (decompressing them if needed), reads every compilation unit
//! header far enough to find `DW_AT_name`, `DW_AT_comp_dir` and
//! `DW_AT_stmt_list`, runs every line program, and keeps a sorted table of
//! address ranges. Build one per object and query it for every diagnostic
//! in that object.
//!
//! # Scope
//!
//! - DWARF 2 to 5, 32- and 64-bit DWARF, either byte order. Unit headers
//!   of every type are understood; type units are skipped. Attributes of
//!   every standard form (and the GNU extensions) are skipped correctly,
//!   including DWARF 5 `strx*`/`addrx*`/`line_strp` forms, and strings
//!   resolve through `.debug_str_offsets`.
//! - Line tables of versions 2 to 5, including the DWARF 5 directory and
//!   file entry formats, `DW_LNE_define_file`, and
//!   `maximum_operations_per_instruction` > 1.
//! - In relocatable objects, `DW_LNE_set_address` operands,
//!   `DW_AT_stmt_list` and string offsets are relocation targets. The
//!   relocation's symbol gives the section (so addresses map to (section
//!   index, offset)); RELA and REL addends are both handled, and RISC-V and
//!   LoongArch `ADD`/`SUB` pairs in `DW_LNS_fixed_advance_pc` and
//!   `DW_LNS_advance_pc` operands are applied.
//! - Split DWARF: a skeleton unit keeps its line table in the object, so
//!   lookups work; the `.dwo` file is never opened.
//!
//! Paths are displayed as GNU ld and `addr2line` display them: a relative
//! file name is joined with its include directory and, if that is
//! relative, with the compilation directory.
//!
//! Malformed units and line programs are skipped, so a partly broken
//! object still gives the lines it can.

mod context;
mod line;
mod reader;
mod unit;

pub use reader::{DwarfError, DwarfResult};

use context::Context;
use line::{Builder, Range};
use reader::Reader;

use crate::diag::SourceLocation;
use crate::elf::read::{ElfFormat, ObjectFile};
use crate::error::Result;

/// The line tables of one object, ready for queries.
#[derive(Debug, Default)]
pub struct LineLookup {
    /// Sorted by (section, start).
    ranges: Vec<Range>,
    files: Vec<String>,
    /// Problems found while parsing (the affected units were skipped).
    errors: Vec<crate::Error>,
}

impl LineLookup {
    /// Parses the line tables of `object`.
    ///
    /// # Errors
    ///
    /// Returns `Error::Malformed` if the object's section table, relocation
    /// sections or compressed debug sections cannot be read. Problems
    /// inside individual units are not errors; see
    /// [`problems`](Self::problems).
    pub fn parse<F: ElfFormat>(object: &ObjectFile<'_, F>) -> Result<Self> {
        let ctx = Context::load(object)?;
        let mut errors = Vec::new();

        let mut units = Vec::new();
        for &info in &ctx.info {
            let (found, error) = unit::parse_units(&ctx, info);
            units.extend(found);
            if let Some(error) = error {
                errors.push(ctx.error(info, error));
            }
        }

        let mut builder = Builder::default();
        for &section in &ctx.line {
            let Some(loaded) = ctx.section(section) else {
                continue;
            };
            let mut r = Reader::new(&loaded.data, ctx.big_endian);
            while !r.is_empty() {
                let start = r.pos() as u64;
                let unit = units.iter().find(|u| {
                    u.stmt_list.is_some_and(|(target, offset)| {
                        offset == start && target.unwrap_or(section) == section
                    })
                });
                let comp_dir = unit.and_then(|u| u.comp_dir.as_deref());
                let name = unit.and_then(|u| u.name.as_deref());
                let before = r.pos();
                if let Err(error) =
                    line::parse_program(&ctx, section, &mut r, comp_dir, name, &mut builder)
                {
                    errors.push(ctx.error(section, error));
                    // The unit length may itself be broken; stop if the
                    // reader could not move past the unit.
                    if r.pos() == before {
                        break;
                    }
                }
            }
        }

        let mut ranges = builder.ranges;
        ranges.sort_by_key(|r| (r.section, r.start, r.end));
        Ok(Self {
            ranges,
            files: builder.files,
            errors,
        })
    }

    /// Finds the source position of `offset` in section `section` (an
    /// index in the object's section header table).
    ///
    /// Returns `None` if no line table covers the position, or it maps to
    /// line 0 (code with no source line).
    #[must_use]
    pub fn find(&self, section: u32, offset: u64) -> Option<SourceLocation> {
        let end = self
            .ranges
            .partition_point(|r| (r.section, r.start) <= (section, offset));
        // Ranges from different sequences may overlap; look back a little
        // for the innermost (latest-starting) one that contains the offset.
        let range = self.ranges[..end]
            .iter()
            .rev()
            .take(32)
            .take_while(|r| r.section == section)
            .find(|r| offset < r.end)?;
        if range.line == 0 {
            return None;
        }
        let file = self.files.get(usize::try_from(range.file).ok()?)?;
        Some(SourceLocation {
            file: file.clone(),
            line: range.line,
        })
    }

    /// Problems found in individual units or line programs, which were
    /// skipped.
    #[must_use]
    pub fn problems(&self) -> &[crate::Error] {
        &self.errors
    }

    /// Whether any line table information was found.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

/// One-shot lookup: parses `object`'s line tables and finds `offset` in
/// section `section`. To look up several positions in one object, build a
/// [`LineLookup`] once instead.
///
/// # Errors
///
/// See [`LineLookup::parse`].
pub fn source_location<F: ElfFormat>(
    object: &ObjectFile<'_, F>,
    section: u32,
    offset: u64,
) -> Result<Option<SourceLocation>> {
    Ok(LineLookup::parse(object)?.find(section, offset))
}
