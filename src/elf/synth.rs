//! Synthetic sections (pipeline stage 9): GOT, IFUNC PLT and its
//! `IRELATIVE` relocations, `.note.gnu.build-id`, `.note.gnu.property`, the
//! linker's `.comment` string and `.eh_frame_hdr`.
//!
//! Planning happens before layout and fixes every size. Contents are written
//! after layout, from final addresses.
//!
//! GOT and IFUNC entries are generic over what needs them: global symbols
//! (by [`SymbolId`], from the scan's flags) and local symbols (by file and
//! symbol index). A static executable has no dynamic relocations except
//! `IRELATIVE`, so every other GOT entry holds a link-time constant.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::{BuildId, LinkOptions};
use crate::elf::read::consts::{
    GNU_PROPERTY_X86_FEATURE_1_AND, GNU_PROPERTY_X86_FEATURE_1_IBT,
    GNU_PROPERTY_X86_FEATURE_1_SHSTK, GNU_PROPERTY_X86_ISA_1_NEEDED, NT_GNU_BUILD_ID,
    NT_GNU_PROPERTY_TYPE_0,
};
use crate::ids::SymbolId;
use crate::output::build_id::build_id_size;
use crate::symbols::{SymbolFlags, SymbolTable};

use super::arch::x86_64::IPLT_ENTRY_SIZE;
use super::inputs::ElfInput;
use super::rules::Synthetic;
use super::scan::{NEEDS_IPLT, ScanResult};

/// A GOT or IFUNC entry owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    /// A global symbol.
    Global(SymbolId),
    /// Local symbol `symbol` of file `file`.
    Local {
        /// The file.
        file: u32,
        /// The symbol index.
        symbol: u32,
    },
}

/// Entries of one table, globals first (by ID), then locals (by file and
/// index).
#[derive(Debug, Default)]
pub struct EntryList {
    globals: Vec<SymbolId>,
    locals: Vec<(u32, u32)>,
}

impl EntryList {
    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.globals.len().saturating_add(self.locals.len())
    }

    /// Whether there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Index of the entry for `owner`.
    #[must_use]
    pub fn index(&self, owner: Owner) -> Option<usize> {
        match owner {
            Owner::Global(id) => self.globals.binary_search(&id).ok(),
            Owner::Local { file, symbol } => self
                .locals
                .binary_search(&(file, symbol))
                .ok()
                .and_then(|i| i.checked_add(self.globals.len())),
        }
    }

    /// All entries, in order.
    pub fn iter(&self) -> impl Iterator<Item = Owner> + '_ {
        self.globals.iter().map(|&id| Owner::Global(id)).chain(
            self.locals
                .iter()
                .map(|&(file, symbol)| Owner::Local { file, symbol }),
        )
    }
}

/// The planned synthetic sections.
#[derive(Debug, Default)]
pub struct Synth {
    /// GOT entries.
    pub got: EntryList,
    /// IFUNC symbols, each with a PLT stub, a `.got.plt` slot and an
    /// `IRELATIVE` relocation.
    pub iplt: EntryList,
    /// Reserved words at the start of `.got.plt` (when the GOT base is used).
    pub got_plt_reserved: u64,
    /// Size of the build-id, if one is written.
    pub build_id: Option<u64>,
    /// The `.note.gnu.property` contents, if any.
    pub property_note: Option<Vec<u8>>,
    /// Whether `.eh_frame_hdr` is written.
    pub eh_frame_hdr: bool,
    /// Number of FDEs in `.eh_frame_hdr`.
    pub fde_count: u64,
    /// Whether `.eh_frame` gets a terminator.
    pub eh_frame_end: bool,
    /// Common block size and alignment.
    pub common: (u64, u64),
}

/// The string the linker adds to `.comment`.
#[must_use]
pub fn comment() -> Vec<u8> {
    let mut text = format!("Linker: {}", crate::version_line()).into_bytes();
    text.push(0);
    text
}

impl Synth {
    /// Plans GOT and IFUNC entries from the scan.
    pub fn plan_entries(&mut self, symbols: &SymbolTable<'_>, scan: &ScanResult) {
        let flagged = |flag: SymbolFlags| -> Vec<SymbolId> {
            symbols
                .ids()
                .collect::<Vec<_>>()
                .into_par_iter()
                .filter(|&id| symbols.flags(id).contains(flag))
                .collect()
        };
        let locals = |pick: fn(&super::scan::FileScan) -> &Vec<u32>| -> Vec<(u32, u32)> {
            scan.files
                .iter()
                .enumerate()
                .flat_map(|(file, result)| {
                    let file = u32::try_from(file).unwrap_or(u32::MAX);
                    pick(result).iter().map(move |&symbol| (file, symbol))
                })
                .collect()
        };
        self.got = EntryList {
            globals: flagged(SymbolFlags::NEEDS_GOT),
            locals: locals(|f| &f.got_locals),
        };
        self.iplt = EntryList {
            globals: flagged(NEEDS_IPLT),
            locals: locals(|f| &f.iplt_locals),
        };
        // A static executable has no dynamic linker to use the reserved
        // `.got.plt` words.
        self.got_plt_reserved = 0;
    }

    /// Size and alignment of a synthetic part.
    #[must_use]
    pub fn size_align(&self, kind: Synthetic) -> (u64, u64) {
        let count = |list: &EntryList| u64::try_from(list.len()).unwrap_or(u64::MAX);
        match kind {
            Synthetic::None => (0, 1),
            Synthetic::BuildId => match self.build_id {
                Some(size) => (16u64.saturating_add(align4(size)), 4),
                None => (0, 1),
            },
            Synthetic::RelaIplt => (count(&self.iplt).saturating_mul(24), 8),
            Synthetic::Iplt => (count(&self.iplt).saturating_mul(IPLT_ENTRY_SIZE), 16),
            Synthetic::EhFrameHdr => {
                if self.eh_frame_hdr {
                    (12u64.saturating_add(self.fde_count.saturating_mul(8)), 4)
                } else {
                    (0, 1)
                }
            }
            Synthetic::EhFrameEnd => (if self.eh_frame_end { 4 } else { 0 }, 4),
            Synthetic::GnuProperty => (
                self.property_note
                    .as_ref()
                    .map_or(0, |n| u64::try_from(n.len()).unwrap_or(0)),
                8,
            ),
            Synthetic::Got => (count(&self.got).saturating_mul(8), 8),
            Synthetic::IgotPlt => {
                let slots = count(&self.iplt).saturating_add(self.got_plt_reserved);
                (slots.saturating_mul(8), 8)
            }
            Synthetic::Common => self.common,
            Synthetic::Comment => (u64::try_from(comment().len()).unwrap_or(0), 1),
        }
    }
}

fn align4(value: u64) -> u64 {
    value.saturating_add(3) & !3
}

/// Plans `.note.gnu.build-id`.
#[must_use]
pub fn plan_build_id(options: &LinkOptions) -> Option<u64> {
    if options.build_id == BuildId::None {
        return None;
    }
    build_id_size(&options.build_id).and_then(|s| u64::try_from(s).ok())
}

/// Merges the inputs' GNU properties into the output note: x86 feature bits
/// are ANDed across inputs (an input without the note has none), ISA levels
/// needed are ORed.
#[must_use]
pub fn plan_property_note(files: &[ElfInput<'_>], options: &LinkOptions) -> Option<Vec<u8>> {
    let mut feature_and: Option<u32> = None;
    let mut isa_needed = 0u32;
    let mut any = false;
    for file in files {
        let Some(object) = &file.object else {
            continue;
        };
        any = true;
        let features = object
            .properties
            .and_then(|p| p.x86_feature_1_and)
            .unwrap_or(0);
        feature_and = Some(feature_and.map_or(features, |f| f & features));
        isa_needed |= object
            .properties
            .and_then(|p| p.x86_isa_1_needed)
            .unwrap_or(0);
    }
    if !any {
        return None;
    }
    let mut features = feature_and.unwrap_or(0);
    if options.x86.ibt {
        features |= GNU_PROPERTY_X86_FEATURE_1_IBT;
    }
    if options.x86.shstk {
        features |= GNU_PROPERTY_X86_FEATURE_1_SHSTK;
    }
    if options.x86.isa_level > 0 {
        isa_needed |= 1u32 << (options.x86.isa_level.saturating_sub(1).min(31));
    }
    let mut properties: Vec<(u32, u32)> = Vec::new();
    if features != 0 {
        properties.push((GNU_PROPERTY_X86_FEATURE_1_AND, features));
    }
    if isa_needed != 0 {
        properties.push((GNU_PROPERTY_X86_ISA_1_NEEDED, isa_needed));
    }
    if properties.is_empty() {
        return None;
    }
    let descsz = u32::try_from(properties.len().saturating_mul(16)).ok()?;
    let mut note = Vec::with_capacity(16usize.saturating_add(descsz as usize));
    note.extend_from_slice(&4u32.to_le_bytes());
    note.extend_from_slice(&descsz.to_le_bytes());
    note.extend_from_slice(&NT_GNU_PROPERTY_TYPE_0.to_le_bytes());
    note.extend_from_slice(b"GNU\0");
    for (kind, value) in properties {
        note.extend_from_slice(&kind.to_le_bytes());
        note.extend_from_slice(&4u32.to_le_bytes());
        note.extend_from_slice(&value.to_le_bytes());
        note.extend_from_slice(&[0; 4]);
    }
    Some(note)
}

/// Writes the header of a build-id note of `size` bytes into `out`; the
/// descriptor is filled in after the image is complete.
pub fn write_build_id_header(out: &mut [u8], size: u64) {
    let descsz = u32::try_from(size).unwrap_or(0);
    let header = [
        4u32.to_le_bytes(),
        descsz.to_le_bytes(),
        NT_GNU_BUILD_ID.to_le_bytes(),
        *b"GNU\0",
    ];
    for (chunk, bytes) in out.as_chunks_mut::<4>().0.iter_mut().zip(header) {
        chunk.copy_from_slice(&bytes);
    }
}
