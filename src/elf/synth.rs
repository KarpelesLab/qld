//! Synthetic sections (pipeline stage 9): GOT and TLS GOT entries, PLTs,
//! `.got.plt`, copy relocation space, the dynamic relocations they need,
//! `.interp`, `.note.gnu.build-id`, `.note.gnu.property`, the linker's
//! `.comment` string and `.eh_frame_hdr`. The dynamic symbol table and its
//! companions are planned by [`super::dynsym`].
//!
//! Planning happens before layout and fixes every size. Contents are written
//! after layout, from final addresses.
//!
//! Entries are generic over what needs them: global symbols (by
//! [`SymbolId`], from the scan's flags) and local symbols (by file and
//! symbol index). The `.got` holds, in order: address entries, TLS
//! module/offset pairs, thread pointer offsets, TLS descriptor pairs, and the
//! module-local TLS pair.
//!
//! **PLT.** A static executable has only IFUNC stubs in `.plt` (with their
//! `.got.plt` slots and `IRELATIVE` relocations in `.rela.plt`). A dynamic
//! output follows GNU ld: `.plt` starts with the lazy-binding header, then
//! one entry per called preemptible function and per IFUNC; with IBT, the
//! entries that code jumps to are in `.plt.sec`. A preemptible function
//! that also has a GOT entry is called through `.plt.got`, which jumps
//! through that GOT entry and needs no `JUMP_SLOT` relocation.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use crate::args::{BuildId, LinkOptions};
use crate::elf::read::consts::{
    GNU_PROPERTY_1_NEEDED, GNU_PROPERTY_AARCH64_FEATURE_1_AND, GNU_PROPERTY_AARCH64_FEATURE_1_BTI,
    GNU_PROPERTY_X86_FEATURE_1_AND, GNU_PROPERTY_X86_FEATURE_1_IBT,
    GNU_PROPERTY_X86_FEATURE_1_SHSTK, GNU_PROPERTY_X86_FEATURE_2_NEEDED,
    GNU_PROPERTY_X86_FEATURE_2_USED, GNU_PROPERTY_X86_ISA_1_NEEDED, GNU_PROPERTY_X86_ISA_1_USED,
    NT_GNU_BUILD_ID, NT_GNU_PROPERTY_TYPE_0,
};
use crate::ids::SymbolId;
use crate::output::build_id::build_id_size;
use crate::symbols::SymbolFlags;

use super::arch::{Arch, DynKind, PltFlags};
use super::export::{Mode, PREEMPTIBLE};
use super::inputs::ElfInput;
use super::refs::{Def, Refs};
use super::rules::Synthetic;
use super::scan::{NEEDS_IPLT, ScanResult};

/// A GOT or PLT entry owner.
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

pub use super::arch::GotKind;

/// A space reserved in the executable for a copy relocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyReloc {
    /// The symbol.
    pub symbol: SymbolId,
    /// Offset in its block (`.bss` or `.data.rel.ro`).
    pub offset: u64,
    /// Size.
    pub size: u64,
    /// In the read-only block.
    pub relro: bool,
}

/// The planned synthetic sections.
#[derive(Debug, Default)]
pub struct Synth {
    /// The architecture, chosen once per link.
    pub arch: Arch,
    /// The output mode.
    pub mode: Option<Mode>,
    /// Address GOT entries.
    pub got: EntryList,
    /// TLS module/offset pairs.
    pub tlsgd: EntryList,
    /// Thread pointer offset entries.
    pub gottpoff: EntryList,
    /// TLS descriptor pairs.
    pub tlsdesc: EntryList,
    /// Whether the module-local TLS pair exists.
    pub tlsld: bool,
    /// IFUNC symbols, each with a PLT stub, a `.got.plt` slot and an
    /// `IRELATIVE` relocation.
    pub iplt: EntryList,
    /// Preemptible functions called through `.plt` (dynamic outputs).
    pub plt: EntryList,
    /// Preemptible functions called through `.plt.got`.
    pub plt_got: EntryList,
    /// Copy relocations, sorted by symbol.
    pub copies: Vec<CopyReloc>,
    /// Symbols that share a copy relocation (an index into `copies`),
    /// sorted by symbol.
    pub copy_aliases: Vec<(SymbolId, usize)>,
    /// Size and alignment of the copy relocation block in `.bss`.
    pub dynbss: (u64, u64),
    /// Size and alignment of the copy relocation block in `.data.rel.ro`.
    pub dynrelro: (u64, u64),
    /// IBT-enabled PLT (x86-64), or BTI-enabled PLT header (AArch64).
    pub ibt: bool,
    /// Reserved words at the start of `.got.plt`.
    pub got_plt_reserved: u64,
    /// Dynamic relocations in `.rela.dyn` that come from GOT entries and
    /// copy relocations, `(relative, other)`.
    pub got_dyn_relocs: (u64, u64),
    /// Dynamic relocations of input sections, `(relative, symbolic)`.
    pub section_dyn_relocs: (u64, u64),
    /// Relative relocations of input sections that `.relr.dyn` can hold.
    pub section_packable: u64,
    /// Relative relocations go to `.relr.dyn` (`-z pack-relative-relocs`
    /// in position-independent output).
    pub relr: bool,
    /// The planned size of `.relr.dyn`, fixed by the layout loop.
    pub relr_size: u64,
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
    /// The program interpreter, NUL-terminated, if `.interp` is written.
    pub interp: Option<Vec<u8>>,
    /// Sizes and alignments of the dynamic linking sections planned by
    /// [`super::dynsym`].
    pub dynamic_sizes: Vec<(Synthetic, u64, u64)>,
    /// Number of `.gnu.version_r` file entries (`sh_info`).
    pub verneed_count: u64,
    /// Number of `.gnu.version_d` entries (`sh_info`).
    pub verdef_count: u64,
}

/// The string the linker adds to `.comment`.
#[must_use]
pub fn comment() -> Vec<u8> {
    let mut text = format!("Linker: {}", crate::version_line()).into_bytes();
    text.push(0);
    text
}

/// The default program interpreter on x86-64 Linux.
pub const DEFAULT_INTERPRETER: &str = "/lib64/ld-linux-x86-64.so.2";

fn u64_len(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

impl Synth {
    /// Whether the output is dynamic.
    #[must_use]
    pub fn dynamic(&self) -> bool {
        self.mode.is_some_and(|m| m.dynamic)
    }

    /// The shape of PLT entries: landing pads for x86-64 IBT or AArch64
    /// BTI. On AArch64 only the header needs one.
    #[must_use]
    pub fn plt_flags(&self) -> PltFlags {
        PltFlags {
            landing_pad: self.ibt,
            entry_landing_pad: self.ibt && self.arch == Arch::X86_64,
        }
    }

    /// Plans GOT, PLT and copy relocation entries from the scan.
    pub fn plan_entries(&mut self, refs: &Refs<'_, '_>, scan: &ScanResult, mode: Mode) {
        let symbols = refs.symbols;
        self.arch = Arch::of_files(refs.files).unwrap_or(self.arch);
        self.mode = Some(mode);
        let all: Vec<SymbolId> = symbols.ids().collect();
        let flagged = |test: &(dyn Fn(SymbolFlags) -> bool + Sync)| -> Vec<SymbolId> {
            all.par_iter()
                .copied()
                .filter(|&id| test(symbols.flags(id)))
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
        let dynamic = mode.dynamic;
        let uses_plt_got = self.arch.uses_plt_got();
        // A preemptible function with both a GOT entry and calls goes
        // through `.plt.got`, unless its PLT entry is its canonical address.
        let plt_got = move |f: SymbolFlags| {
            dynamic
                && uses_plt_got
                && f.contains(SymbolFlags::NEEDS_PLT | SymbolFlags::NEEDS_GOT)
                && !f.contains(SymbolFlags::NEEDS_CANONICAL_PLT)
        };
        self.got = EntryList {
            globals: flagged(&|f| f.contains(SymbolFlags::NEEDS_GOT)),
            locals: locals(|f| &f.got_locals),
        };
        self.tlsgd = EntryList {
            globals: flagged(&|f| f.contains(SymbolFlags::NEEDS_TLSGD)),
            locals: locals(|f| &f.tlsgd_locals),
        };
        self.gottpoff = EntryList {
            globals: flagged(&|f| f.contains(SymbolFlags::NEEDS_GOTTPOFF)),
            locals: locals(|f| &f.gottpoff_locals),
        };
        self.tlsdesc = EntryList {
            globals: flagged(&|f| f.contains(SymbolFlags::NEEDS_TLSDESC)),
            locals: locals(|f| &f.tlsdesc_locals),
        };
        self.tlsld = scan.tls_ld();
        self.iplt = EntryList {
            globals: flagged(&|f| f.contains(NEEDS_IPLT)),
            locals: locals(|f| &f.iplt_locals),
        };
        self.plt = EntryList {
            globals: flagged(&|f| {
                dynamic
                    && f.contains(SymbolFlags::NEEDS_PLT | PREEMPTIBLE)
                    && !f.contains(NEEDS_IPLT)
                    && !plt_got(f)
            }),
            locals: Vec::new(),
        };
        self.plt_got = EntryList {
            globals: flagged(&|f| plt_got(f) && !f.contains(NEEDS_IPLT)),
            locals: Vec::new(),
        };
        self.plan_copies(
            refs,
            flagged(&|f| f.contains(SymbolFlags::NEEDS_COPY_RELOC)),
        );
        let has_got_plt = !self.got.is_empty()
            || !self.plt.is_empty()
            || !self.iplt.is_empty()
            || !self.plt_got.is_empty()
            || !self.tlsgd.is_empty()
            || !self.gottpoff.is_empty()
            || !self.tlsdesc.is_empty()
            || self.tlsld
            || scan.uses_got_base();
        // A static executable has no dynamic linker to use the reserved
        // `.got.plt` words.
        self.got_plt_reserved = if dynamic && has_got_plt {
            self.arch.got_plt_reserved()
        } else {
            0
        };
        self.section_dyn_relocs = scan.section_dyn_relocs();
        self.section_packable = scan.section_packable();
        self.got_dyn_relocs = self.count_got_relocs(refs);
    }

    /// Allocates space for copy relocations, in symbol order.
    ///
    /// Symbols that share a definition (the same section and value in the
    /// same shared library, such as glibc's `environ` and `__environ`) share
    /// one copy: every alias the library defines becomes an alias of the
    /// copy and is exported, so the library binds all of them to it, as lld
    /// does.
    fn plan_copies(&mut self, refs: &Refs<'_, '_>, symbols: Vec<SymbolId>) {
        // (file, shndx, value) of each symbol's definition.
        let key = |id: SymbolId| -> Option<(usize, u16, u64)> {
            let def = refs.symbols.definition(id);
            let shared = refs.files.get(def.file.index())?.shared.as_ref()?;
            let index = *shared.symbols.get(def.index as usize)?;
            let raw = shared.elf.symbols().get_raw(index as usize)?;
            Some((def.file.index(), raw.st_shndx, raw.st_value))
        };
        let mut bss = (0u64, 1u64);
        let mut relro = (0u64, 1u64);
        let mut copies: Vec<CopyReloc> = Vec::with_capacity(symbols.len());
        let mut keys: Vec<((usize, u16, u64), usize)> = Vec::with_capacity(symbols.len());
        let mut aliases: Vec<(SymbolId, usize)> = Vec::new();
        for id in symbols {
            let symbol_key = key(id);
            if let Some(symbol_key) = symbol_key
                && let Some(&(_, copy)) = keys.iter().find(|(k, _)| *k == symbol_key)
            {
                aliases.push((id, copy));
                continue;
            }
            let (size, align, read_only) = copy_shape(refs, id);
            let block = if read_only { &mut relro } else { &mut bss };
            let mask = align.wrapping_sub(1);
            let offset = block.0.checked_add(mask).map_or(block.0, |v| v & !mask);
            block.0 = offset.saturating_add(size);
            block.1 = block.1.max(align);
            if let Some(symbol_key) = symbol_key {
                keys.push((symbol_key, copies.len()));
            }
            copies.push(CopyReloc {
                symbol: id,
                offset,
                size,
                relro: read_only,
            });
        }
        // Other symbols the libraries define at the copied addresses.
        keys.sort_unstable();
        let mut files: Vec<usize> = keys.iter().map(|((file, _, _), _)| *file).collect();
        files.dedup();
        for file in files {
            let Some(shared) = refs.files.get(file).and_then(|f| f.shared.as_ref()) else {
                continue;
            };
            let ids = refs.resolution.symbol_ids(crate::ids::FileId::new(file));
            for (local, (&index, &id)) in shared.symbols.iter().zip(ids).enumerate() {
                // The versioned names of the same definitions add nothing.
                if !matches!(
                    shared.uses.get(local),
                    Some(crate::symbols::SymbolUse::Definition { .. })
                ) || refs.symbols.name(id).version().is_some()
                {
                    continue;
                }
                let Some(raw) = shared.elf.symbols().get_raw(index as usize) else {
                    continue;
                };
                let Ok(at) =
                    keys.binary_search_by_key(&(file, raw.st_shndx, raw.st_value), |(k, _)| *k)
                else {
                    continue;
                };
                let def = refs.symbols.definition(id);
                let copy = keys.get(at).map_or(0, |(_, copy)| *copy);
                if def.file.index() == file && copies.get(copy).is_some_and(|c| c.symbol != id) {
                    aliases.push((id, copy));
                }
            }
        }
        aliases.sort_unstable();
        aliases.dedup_by_key(|(id, _)| *id);
        self.copies = copies;
        self.copy_aliases = aliases;
        self.dynbss = bss;
        self.dynrelro = relro;
    }

    /// The copy relocation `id` has or shares, if any.
    #[must_use]
    pub fn copy_of(&self, id: SymbolId) -> Option<&CopyReloc> {
        if let Ok(at) = self.copies.binary_search_by_key(&id, |c| c.symbol) {
            return self.copies.get(at);
        }
        let at = self
            .copy_aliases
            .binary_search_by_key(&id, |(alias, _)| *alias)
            .ok()?;
        let &(_, copy) = self.copy_aliases.get(at)?;
        self.copies.get(copy)
    }

    /// Counts the `.rela.dyn` relocations of GOT entries and copies:
    /// `(relative, other)`.
    fn count_got_relocs(&self, refs: &Refs<'_, '_>) -> (u64, u64) {
        let Some(mode) = self.mode.filter(|m| m.dynamic) else {
            return (0, 0);
        };
        let mut relative = 0u64;
        let mut other = 0u64;
        let mut add = |reloc: SlotReloc| match reloc {
            SlotReloc::None => {}
            SlotReloc::Relative => relative = relative.saturating_add(1),
            SlotReloc::Symbolic(_) | SlotReloc::Module(_) => other = other.saturating_add(1),
        };
        for (list, kind) in [
            (&self.got, GotKind::Address),
            (&self.tlsgd, GotKind::TlsGd),
            (&self.gottpoff, GotKind::TpOff),
            (&self.tlsdesc, GotKind::TlsDesc),
        ] {
            for owner in list.iter() {
                let [first, second] = got_slot_relocs(refs, mode, owner, kind);
                add(first);
                add(second);
            }
        }
        if self.tlsld {
            add(SlotReloc::Module(DynKind::DtpMod));
        }
        other = other.saturating_add(u64_len(self.copies.len()));
        // IFUNC slots whose IRELATIVE relocations go to `.rela.dyn`.
        if !self.arch.irelative_in_rela_plt() {
            other = other.saturating_add(u64_len(self.iplt.len()));
        }
        (relative, other)
    }

    /// Number of `.rela.dyn` entries.
    #[must_use]
    pub fn rela_dyn_count(&self) -> u64 {
        self.got_dyn_relocs
            .0
            .saturating_add(self.got_dyn_relocs.1)
            .saturating_add(self.section_dyn_relocs.0)
            .saturating_add(self.section_dyn_relocs.1)
            .saturating_sub(self.relr_count())
    }

    /// Number of `R_X86_64_RELATIVE` relocations in `.rela.dyn`.
    #[must_use]
    pub fn relative_count(&self) -> u64 {
        self.got_dyn_relocs
            .0
            .saturating_add(self.section_dyn_relocs.0)
            .saturating_sub(self.relr_count())
    }

    /// Number of relative relocations packed into `.relr.dyn`: the GOT's
    /// (whose entries are all word-aligned) and the packable ones of input
    /// sections, when `-z pack-relative-relocs` applies.
    #[must_use]
    pub fn relr_count(&self) -> u64 {
        if self.relr {
            self.got_dyn_relocs.0.saturating_add(self.section_packable)
        } else {
            0
        }
    }

    /// Number of words the GOT occupies.
    #[must_use]
    pub fn got_words(&self) -> u64 {
        self.arch
            .got_header_words()
            .saturating_add(u64_len(self.got.len()))
            .saturating_add(u64_len(self.tlsgd.len()).saturating_mul(2))
            .saturating_add(u64_len(self.gottpoff.len()))
            .saturating_add(u64_len(self.tlsdesc.len()).saturating_mul(2))
            .saturating_add(if self.tlsld { 2 } else { 0 })
    }

    /// The first GOT word of each kind of entry.
    #[must_use]
    pub fn got_base_word(&self, kind: GotKind) -> u64 {
        let header = self.arch.got_header_words();
        let address = header.saturating_add(u64_len(self.got.len()));
        let tlsgd = address.saturating_add(u64_len(self.tlsgd.len()).saturating_mul(2));
        let tpoff = tlsgd.saturating_add(u64_len(self.gottpoff.len()));
        let desc = tpoff.saturating_add(u64_len(self.tlsdesc.len()).saturating_mul(2));
        match kind {
            GotKind::Address => header,
            GotKind::TlsGd => address,
            GotKind::TpOff => tlsgd,
            GotKind::TlsDesc => tpoff,
            GotKind::TlsLd => desc,
        }
    }

    /// The GOT word of `owner`'s entry of `kind`.
    #[must_use]
    pub fn got_word(&self, owner: Owner, kind: GotKind) -> Option<u64> {
        let (list, width) = match kind {
            GotKind::Address => (&self.got, 1u64),
            GotKind::TlsGd => (&self.tlsgd, 2),
            GotKind::TpOff => (&self.gottpoff, 1),
            GotKind::TlsDesc => (&self.tlsdesc, 2),
            GotKind::TlsLd => return self.tlsld.then(|| self.got_base_word(GotKind::TlsLd)),
        };
        let index = u64::try_from(list.index(owner)?).ok()?;
        self.got_base_word(kind)
            .checked_add(index.checked_mul(width)?)
    }

    /// Number of PLT entries in `.plt` (and `.plt.sec`) of a dynamic output:
    /// preemptible functions, then IFUNCs.
    #[must_use]
    pub fn plt_entries(&self) -> u64 {
        u64_len(self.plt.len()).saturating_add(u64_len(self.iplt.len()))
    }

    /// The PLT index (in `.plt` entries and `.got.plt` slots) of `owner`.
    #[must_use]
    pub fn plt_index(&self, owner: Owner) -> Option<u64> {
        if let Some(index) = self.plt.index(owner) {
            return u64::try_from(index).ok();
        }
        let index = u64::try_from(self.iplt.index(owner)?).ok()?;
        if self.dynamic() {
            index.checked_add(u64_len(self.plt.len()))
        } else {
            Some(index)
        }
    }

    /// Size and alignment of a synthetic part.
    #[must_use]
    pub fn size_align(&self, kind: Synthetic) -> (u64, u64) {
        let count = |list: &EntryList| u64_len(list.len());
        let dynamic = self.dynamic();
        match kind {
            Synthetic::None => (0, 1),
            Synthetic::BuildId => match self.build_id {
                Some(size) => (16u64.saturating_add(align4(size)), 4),
                None => (0, 1),
            },
            Synthetic::Interp => (self.interp.as_ref().map_or(0, |i| u64_len(i.len())), 1),
            Synthetic::Hash
            | Synthetic::GnuHash
            | Synthetic::DynSym
            | Synthetic::DynStr
            | Synthetic::VerSym
            | Synthetic::VerDef
            | Synthetic::VerNeed
            | Synthetic::Dynamic => self
                .dynamic_sizes
                .iter()
                .find(|(k, ..)| *k == kind)
                .map_or((0, 1), |&(_, size, align)| (size, align)),
            Synthetic::RelaDyn => (self.rela_dyn_count().saturating_mul(24), 8),
            Synthetic::RelrDyn => {
                if self.relr_count() > 0 {
                    (self.relr_size, 8)
                } else {
                    (0, 8)
                }
            }
            Synthetic::RelaPlt => {
                let entries = if dynamic && !self.arch.irelative_in_rela_plt() {
                    u64_len(self.plt.len())
                } else if dynamic {
                    self.plt_entries()
                } else {
                    count(&self.iplt)
                };
                (entries.saturating_mul(24), 8)
            }
            Synthetic::Plt => {
                let flags = self.plt_flags();
                let align = self.arch.plt_align();
                if dynamic {
                    // GNU ld keeps the lazy PLT header when only `.plt.got`
                    // entries exist.
                    let entries = self.plt_entries();
                    if entries == 0 && self.plt_got.is_empty() {
                        (0, align)
                    } else {
                        (
                            self.arch.plt_header_size(flags).saturating_add(
                                entries.saturating_mul(self.arch.plt_entry_size(flags)),
                            ),
                            align,
                        )
                    }
                } else {
                    (
                        count(&self.iplt).saturating_mul(self.arch.iplt_entry_size()),
                        align,
                    )
                }
            }
            Synthetic::PltSec => {
                // x86-64 IBT splits the PLT in two, and PowerPC64 calls
                // through stubs there.
                if dynamic && self.arch.has_plt_sec(self.ibt) {
                    (
                        self.plt_entries()
                            .saturating_mul(self.arch.plt_sec_entry_size(self.plt_flags())),
                        self.arch.plt_align(),
                    )
                } else {
                    (0, self.arch.plt_align())
                }
            }
            Synthetic::PltGot => {
                let entry = self.arch.plt_got_entry_size(self.plt_flags());
                let align = if entry >= 16 { 16 } else { 8 };
                (count(&self.plt_got).saturating_mul(entry), align)
            }
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
            Synthetic::Got => (self.got_words().saturating_mul(8), 8),
            Synthetic::GotPlt => {
                let slots = if dynamic {
                    self.plt_entries()
                } else {
                    count(&self.iplt)
                };
                (
                    slots
                        .saturating_add(self.got_plt_reserved)
                        .saturating_mul(8),
                    8,
                )
            }
            Synthetic::DynBss => self.dynbss,
            Synthetic::DynRelro => self.dynrelro,
            Synthetic::Common => self.common,
            Synthetic::Comment => (u64::try_from(comment().len()).unwrap_or(0), 1),
        }
    }
}

/// Size, alignment and read-only-ness of the space a copy relocation of
/// `id` needs, from the shared library's definition.
fn copy_shape(refs: &Refs<'_, '_>, id: SymbolId) -> (u64, u64, bool) {
    use crate::elf::read::consts::SHF_WRITE;
    let def = refs.symbols.definition(id);
    let Some(shared) = refs
        .files
        .get(def.file.index())
        .and_then(|f| f.shared.as_ref())
    else {
        return (0, 1, false);
    };
    let Some(raw) = shared
        .symbols
        .get(def.index as usize)
        .and_then(|&index| shared.elf.symbols().get_raw(index as usize))
    else {
        return (0, 1, false);
    };
    let section = shared
        .elf
        .elf()
        .section_header(u32::from(raw.st_shndx))
        .ok();
    let section_align = section.map_or(1, |s| s.sh_addralign.max(1));
    let value_align = if raw.st_value == 0 {
        u64::MAX
    } else {
        1u64.checked_shl(raw.st_value.trailing_zeros())
            .unwrap_or(u64::MAX)
    };
    let align = section_align.min(value_align).clamp(1, 1 << 20);
    let align = if align.is_power_of_two() { align } else { 1 };
    // A copy of data in a read-only (RELRO) part of the library belongs in
    // the executable's RELRO region too.
    let read_only = section.is_some_and(|s| s.sh_flags & SHF_WRITE == 0);
    (raw.st_size, align, read_only)
}

/// The dynamic relocation one GOT word needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotReloc {
    /// None: the word is final at link time.
    None,
    /// A relative relocation.
    Relative,
    /// A relocation of this kind against the owner's dynamic symbol.
    Symbolic(DynKind),
    /// A relocation of this kind against symbol 0 (the output itself).
    Module(DynKind),
}

/// The dynamic relocations of the (one or two) GOT words of `owner`'s
/// entry of `kind`.
#[must_use]
pub fn got_slot_relocs(
    refs: &Refs<'_, '_>,
    mode: Mode,
    owner: Owner,
    kind: GotKind,
) -> [SlotReloc; 2] {
    if !mode.dynamic {
        return [SlotReloc::None; 2];
    }
    if kind == GotKind::TlsLd {
        return [SlotReloc::Module(DynKind::DtpMod), SlotReloc::None];
    }
    let target = match owner {
        Owner::Global(id) => Some(refs.global_target(id, true)),
        Owner::Local { file, symbol } => refs.target(file as usize, symbol as usize),
    };
    let Some(target) = target else {
        return [SlotReloc::None; 2];
    };
    let flags = target
        .global
        .map_or(SymbolFlags::EMPTY, |id| refs.symbols.flags(id));
    let preemptible = target.global.is_some() && flags.contains(PREEMPTIBLE);
    let defined = !matches!(target.def, Def::Undefined { .. } | Def::Shared(_));
    let absolute =
        matches!(target.def, Def::Absolute(_)) || flags.contains(super::defined::ABSOLUTE);
    match kind {
        GotKind::Address => {
            if preemptible {
                [SlotReloc::Symbolic(DynKind::GlobDat), SlotReloc::None]
            } else if mode.pic && defined && !absolute {
                [SlotReloc::Relative, SlotReloc::None]
            } else {
                [SlotReloc::None; 2]
            }
        }
        GotKind::TpOff => {
            if preemptible {
                [SlotReloc::Symbolic(DynKind::TpOff), SlotReloc::None]
            } else if mode.shared {
                [SlotReloc::Module(DynKind::TpOff), SlotReloc::None]
            } else {
                [SlotReloc::None; 2]
            }
        }
        GotKind::TlsGd => {
            if preemptible {
                [
                    SlotReloc::Symbolic(DynKind::DtpMod),
                    SlotReloc::Symbolic(DynKind::DtpOff),
                ]
            } else if mode.shared {
                [SlotReloc::Module(DynKind::DtpMod), SlotReloc::None]
            } else {
                [SlotReloc::None; 2]
            }
        }
        GotKind::TlsDesc => {
            if preemptible {
                [SlotReloc::Symbolic(DynKind::TlsDesc), SlotReloc::None]
            } else {
                [SlotReloc::Module(DynKind::TlsDesc), SlotReloc::None]
            }
        }
        GotKind::TlsLd => [SlotReloc::Module(DynKind::DtpMod), SlotReloc::None],
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

/// The architecture feature bits every regular object has (0 without
/// objects): x86 `FEATURE_1_AND` or AArch64 `FEATURE_1_AND`, whichever the
/// link targets.
#[must_use]
pub fn input_features(files: &[ElfInput<'_>]) -> u32 {
    let aarch64 = Arch::of_files(files) == Some(Arch::AArch64);
    let mut feature_and: Option<u32> = None;
    for file in files {
        let Some(object) = &file.object else {
            continue;
        };
        let properties = object.properties.unwrap_or_default();
        let features = if aarch64 {
            properties.aarch64_feature_1_and.unwrap_or(0)
        } else {
            properties.x86_feature_1_and.unwrap_or(0)
        };
        feature_and = Some(feature_and.map_or(features, |f| f & features));
    }
    feature_and.unwrap_or(0)
}

/// Whether PLT entries carry a landing pad: x86-64 IBT (`-z ibtplt`,
/// `-z ibt`, or every object marked) or AArch64 BTI (every object marked,
/// or `-z force-bti`).
#[must_use]
pub fn plan_ibt(files: &[ElfInput<'_>], options: &LinkOptions) -> bool {
    if Arch::of_files(files) == Some(Arch::AArch64) {
        return input_features(files) & GNU_PROPERTY_AARCH64_FEATURE_1_BTI != 0;
    }
    options.x86.ibtplt
        || options.x86.ibt
        || input_features(files) & GNU_PROPERTY_X86_FEATURE_1_IBT != 0
}

/// Merges the inputs' GNU properties into the output note, as GNU ld does:
///
/// - x86 feature bits (`FEATURE_1_AND`) are ANDed across inputs (an input
///   without the note has none), plus `-z ibt` and `-z shstk`;
/// - "needed" bits (`GNU_PROPERTY_1_NEEDED`, x86 `ISA_1_NEEDED` and
///   `FEATURE_2_NEEDED`) are ORed, plus the `-z x86-64-vN` level;
/// - "used" bits (x86 `ISA_1_USED` and `FEATURE_2_USED`) are ORed, but kept
///   only when every input has them.
///
/// Properties are written in type order; zero values are left out. Shared
/// libraries do not take part.
#[must_use]
pub fn plan_property_note(files: &[ElfInput<'_>], options: &LinkOptions) -> Option<Vec<u8>> {
    let aarch64 = Arch::of_files(files) == Some(Arch::AArch64);
    let mut needed_1 = 0u32;
    let mut isa_needed = 0u32;
    let mut feature_2_needed = 0u32;
    let mut isa_used: Option<u32> = None;
    let mut feature_2_used: Option<u32> = None;
    let mut all_used = (true, true);
    let mut any = false;
    for file in files {
        let Some(object) = &file.object else {
            continue;
        };
        any = true;
        let properties = object.properties.unwrap_or_default();
        needed_1 |= properties.needed_1.unwrap_or(0);
        isa_needed |= properties.x86_isa_1_needed.unwrap_or(0);
        feature_2_needed |= properties.x86_feature_2_needed.unwrap_or(0);
        match properties.x86_isa_1_used {
            Some(bits) => isa_used = Some(isa_used.unwrap_or(0) | bits),
            None => all_used.0 = false,
        }
        match properties.x86_feature_2_used {
            Some(bits) => feature_2_used = Some(feature_2_used.unwrap_or(0) | bits),
            None => all_used.1 = false,
        }
    }
    if !any {
        return None;
    }
    let features = input_features(files);
    if aarch64 {
        // AArch64 has one feature word; the x86 properties do not apply.
        let properties: Vec<(u32, u32)> = [
            (GNU_PROPERTY_1_NEEDED, needed_1),
            (GNU_PROPERTY_AARCH64_FEATURE_1_AND, features),
        ]
        .into_iter()
        .filter(|&(_, value)| value != 0)
        .collect();
        return encode_property_note(&properties);
    }
    let mut features = features;
    if options.x86.ibt {
        features |= GNU_PROPERTY_X86_FEATURE_1_IBT;
    }
    if options.x86.shstk {
        features |= GNU_PROPERTY_X86_FEATURE_1_SHSTK;
    }
    if options.x86.isa_level > 0 {
        isa_needed |= 1u32 << (options.x86.isa_level.saturating_sub(1).min(31));
    }
    let used = |merged: Option<u32>, all: bool| if all { merged.unwrap_or(0) } else { 0 };
    let properties: Vec<(u32, u32)> = [
        (GNU_PROPERTY_1_NEEDED, needed_1),
        (GNU_PROPERTY_X86_FEATURE_1_AND, features),
        (GNU_PROPERTY_X86_FEATURE_2_NEEDED, feature_2_needed),
        (GNU_PROPERTY_X86_ISA_1_NEEDED, isa_needed),
        (
            GNU_PROPERTY_X86_FEATURE_2_USED,
            used(feature_2_used, all_used.1),
        ),
        (GNU_PROPERTY_X86_ISA_1_USED, used(isa_used, all_used.0)),
    ]
    .into_iter()
    .filter(|&(_, value)| value != 0)
    .collect();
    encode_property_note(&properties)
}

/// Encodes `.note.gnu.property` from `(type, value)` pairs, in type order.
fn encode_property_note(properties: &[(u32, u32)]) -> Option<Vec<u8>> {
    if properties.is_empty() {
        return None;
    }
    let descsz = u32::try_from(properties.len().saturating_mul(16)).ok()?;
    let mut note = Vec::with_capacity(16usize.saturating_add(descsz as usize));
    note.extend_from_slice(&4u32.to_le_bytes());
    note.extend_from_slice(&descsz.to_le_bytes());
    note.extend_from_slice(&NT_GNU_PROPERTY_TYPE_0.to_le_bytes());
    note.extend_from_slice(b"GNU\0");
    for &(kind, value) in properties {
        note.extend_from_slice(&kind.to_le_bytes());
        note.extend_from_slice(&4u32.to_le_bytes());
        note.extend_from_slice(&value.to_le_bytes());
        note.extend_from_slice(&[0; 4]);
    }
    Some(note)
}

/// Plans `.interp`: the `--dynamic-linker` path, or the default.
#[must_use]
pub fn plan_interp(options: &LinkOptions, mode: Mode, arch: Arch) -> Option<Vec<u8>> {
    if !mode.interp {
        return None;
    }
    let mut path = options.dynamic_linker.as_ref().map_or_else(
        || arch.default_interpreter().as_bytes().to_vec(),
        |p| p.as_os_str().as_encoded_bytes().to_vec(),
    );
    path.push(0);
    Some(path)
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
