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
use crate::debug::debug_names::DebugNames;
use crate::debug::section::{OutputCompression, compress_section};
use crate::elf::read::Elf64Le;

/// The debug indexes of a link, planned before layout.
#[derive(Debug, Default)]
pub struct DebugIndexes<'a> {
    gdb_index: Option<GdbIndex<'a>>,
    debug_names: Option<DebugNames>,
    /// `.debug_names` compressed for `--compress-debug-sections`.
    compressed_names: Option<Vec<u8>>,
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
        if !options.gdb_index && !options.debug_names {
            return Ok(this);
        }
        let objects = live_objects(files, resolution);
        let live = |file: usize, section: u32| sections.is_present_in(file, section);
        if options.gdb_index {
            this.gdb_index = Some(GdbIndex::build(&objects, &live)?);
        }
        if options.debug_names {
            this.debug_names = Some(DebugNames::build(&objects, &live)?);
        }
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
        // The merged `.debug_names` replaces the inputs'.
        if let Some(names) = &self.debug_names {
            for (file, object) in live_objects(files, resolution) {
                for (index, section) in object.sections.iter().enumerate() {
                    if section.name == b".debug_names"
                        && !section.is_alloc()
                        && let Some(id) =
                            sections.id(file, u32::try_from(index).unwrap_or(u32::MAX))
                        && let Some(slot) = sections.live.get_mut(id.index())
                    {
                        *slot = false;
                    }
                }
            }
            if names.is_empty() {
                self.debug_names = None;
            }
        }
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
        if let Some(bytes) = &self.compressed_names {
            return bytes.len() as u64;
        }
        self.debug_names.as_ref().map_or(0, DebugNames::size)
    }

    /// Renders and compresses `.debug_names` for `--compress-debug-sections`
    /// (its contents depend only on offsets within other debug sections,
    /// which compression does not change). Returns whether it is gABI
    /// compressed (`Some(true)`) or `zlib-gnu` (`Some(false)`), or `None`
    /// when there is no section or compression would not make it smaller.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Limit`] for oversized sections.
    pub fn compress(
        &mut self,
        addresses: &Addresses<'_, '_>,
        compression: OutputCompression,
    ) -> Result<Option<bool>> {
        let Some(names) = &self.debug_names else {
            return Ok(None);
        };
        let offset = |file: usize, section: u32, value: u64| {
            addresses.section_offset_address(file, section, value)
        };
        let bytes = names.render(&offset)?;
        let compressed = compress_section::<Elf64Le>(&bytes, compression, 4);
        if compressed.len() >= bytes.len() {
            return Ok(None);
        }
        self.compressed_names = Some(compressed);
        Ok(Some(compression.is_gabi()))
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
        if let Some(names) = &self.debug_names {
            let position = generated_position(addresses, b".debug_names")?;
            let offset = |file: usize, section: u32, value: u64| {
                addresses.section_offset_address(file, section, value)
            };
            let (bytes, compressed) = match &self.compressed_names {
                Some(bytes) => (bytes.clone(), true),
                None => (names.render(&offset)?, false),
            };
            prerendered.push(Prerendered {
                position,
                bytes,
                compressed,
            });
        }
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
        .position(|s| {
            s.trailer == Trailer::Generated
                && (s.name == name || (s.name_prefix == b".z" && name.get(1..) == Some(s.name)))
        })
        .and_then(|p| u32::try_from(p).ok())
        .ok_or_else(|| {
            crate::Error::Internal(format!(
                "layout has no {} section",
                String::from_utf8_lossy(name)
            ))
        })
}
