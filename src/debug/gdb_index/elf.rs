//! The ELF link driver's side of `--gdb-index` and `--debug-names`: which
//! inputs are read, which input sections the indexes consume, and where
//! the rendered sections go.

use crate::args::LinkOptions;
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::elf::inputs::ElfInput;
use crate::elf::layout::Trailer;
use crate::elf::object::ObjectInput;
use crate::elf::sections::Sections;
use crate::elf::values::Addresses;
use crate::elf::write::Prerendered;
use crate::error::Result;
use crate::ids::FileId;
use crate::symbols::Resolution;

use super::GdbIndex;

/// The debug indexes of a link, planned before layout.
#[derive(Debug, Default)]
pub struct DebugIndexes<'a> {
    gdb_index: Option<GdbIndex<'a>>,
}

impl<'a> DebugIndexes<'a> {
    /// Reads what the indexes need from the inputs. Call it once
    /// `--gc-sections` has run (ICF may come before or after: sections it
    /// folds count as live); it only reads, so it can run alongside the
    /// relocation scan. Then call [`apply`](Self::apply).
    ///
    /// # Errors
    ///
    /// Returns errors for unreadable inputs and oversized indexes.
    pub fn build(
        files: &[ElfInput<'a>],
        resolution: &Resolution<'_>,
        sections: &Sections,
        options: &LinkOptions,
    ) -> Result<Self> {
        let mut this = Self::default();
        if !options.gdb_index {
            return Ok(this);
        }
        let objects = live_objects(files, resolution);
        let live = |file: usize, section: u32| sections.is_present_in(file, section);
        this.gdb_index = Some(GdbIndex::build(&objects, &live)?);
        Ok(this)
    }

    /// Reports the problems found in the inputs, and drops the input
    /// sections the indexes consume from `sections`.
    pub fn apply(
        &mut self,
        files: &[ElfInput<'a>],
        resolution: &Resolution<'_>,
        sections: &mut Sections,
        diagnostics: &dyn DiagnosticSink,
    ) {
        let Some(index) = &self.gdb_index else {
            return;
        };
        for (file, section, problem) in &index.problems {
            let (name, object) = files
                .get(*file)
                .map(|f| (f.display(), f.object.as_ref()))
                .unwrap_or_default();
            let section_name = object
                .and_then(|o| o.section(*section))
                .map_or_else(String::new, |s| {
                    String::from_utf8_lossy(s.name).into_owned()
                });
            diagnostics.emit(Diagnostic::warning(format!(
                "{name}:({section_name}): malformed DWARF ({} at offset {:#x}); --gdb-index skips the rest",
                problem.what, problem.offset
            )));
        }
        // `.debug_gnu_pub{names,types}` exist only to build the index.
        for (file, object) in live_objects(files, resolution) {
            for (index, section) in object.sections.iter().enumerate() {
                if super::is_consumed(section.name)
                    && let Some(id) = sections.id(file, u32::try_from(index).unwrap_or(u32::MAX))
                    && let Some(slot) = sections.live.get_mut(id.index())
                {
                    *slot = false;
                }
            }
        }
        if index.is_empty() {
            self.gdb_index = None;
        }
    }

    /// The size of `.debug_names` (0: none).
    #[must_use]
    pub fn debug_names_size(&self) -> u64 {
        0
    }

    /// The size of `.gdb_index` (0: none).
    #[must_use]
    pub fn gdb_index_size(&self) -> u64 {
        self.gdb_index.as_ref().map_or(0, GdbIndex::size)
    }

    /// Renders the indexes with the final addresses into `prerendered`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Internal`] if layout did not reserve them.
    pub fn render(
        &self,
        addresses: &Addresses<'_, '_>,
        prerendered: &mut Vec<Prerendered>,
    ) -> Result<()> {
        let Some(index) = &self.gdb_index else {
            return Ok(());
        };
        let position = generated_position(addresses, b".gdb_index")?;
        let offset = |file: usize, section: u32| addresses.section_offset_address(file, section, 0);
        let bytes = index.render(&offset, &offset)?;
        prerendered.push(Prerendered {
            position,
            bytes,
            compressed: false,
        });
        Ok(())
    }
}

/// The live objects of the link, as (file index, object).
fn live_objects<'x, 'a>(
    files: &'x [ElfInput<'a>],
    resolution: &Resolution<'_>,
) -> Vec<(usize, &'x ObjectInput<'a>)> {
    files
        .iter()
        .enumerate()
        .filter(|(index, _)| resolution.is_live(FileId::new(*index)))
        .filter_map(|(index, file)| Some((index, file.object.as_ref()?)))
        .collect()
}

/// The position in the layout of the generated section `name`.
fn generated_position(addresses: &Addresses<'_, '_>, name: &[u8]) -> Result<u32> {
    addresses
        .layout
        .sections
        .iter()
        .position(|s| s.trailer == Trailer::Generated && s.name == name)
        .and_then(|p| u32::try_from(p).ok())
        .ok_or_else(|| {
            crate::Error::Internal(format!(
                "layout has no {} section",
                String::from_utf8_lossy(name)
            ))
        })
}
