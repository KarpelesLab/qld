//! The Mach-O header, the load commands and `__LINKEDIT`.
//!
//! Load commands come in lld's order (which follows ld64): segments, the
//! dyld information (`LC_DYLD_CHAINED_FIXUPS` and `LC_DYLD_EXPORTS_TRIE`, or
//! `LC_DYLD_INFO_ONLY`), `LC_SYMTAB`, `LC_DYSYMTAB`, `LC_RPATH`s,
//! `LC_LOAD_DYLINKER` or `LC_ID_DYLIB`, `LC_UUID`, `LC_BUILD_VERSION`,
//! `LC_MAIN`, the dylib loads, `LC_FUNCTION_STARTS`, `LC_DATA_IN_CODE` and
//! `LC_CODE_SIGNATURE`. `__LINKEDIT` holds, in order, the fixups, the export
//! trie, function starts, data in code, the symbol table, the indirect
//! symbol table, the string table and the code signature.

#![deny(clippy::arithmetic_side_effects)]

use crate::args::darwin::LoadMode;
use crate::args::darwin::MachOutputType;
use crate::error::{Error, Result};
use crate::macho::read::consts::{
    CPU_SUBTYPE_ARM64_ALL, CPU_SUBTYPE_LIB64, CPU_SUBTYPE_X86_64_ALL, CPU_TYPE_ARM64,
    CPU_TYPE_X86_64, LC_BUILD_VERSION, LC_CODE_SIGNATURE, LC_DATA_IN_CODE, LC_DYLD_CHAINED_FIXUPS,
    LC_DYLD_EXPORTS_TRIE, LC_DYLD_INFO_ONLY, LC_DYSYMTAB, LC_FUNCTION_STARTS, LC_ID_DYLIB,
    LC_LOAD_DYLIB, LC_LOAD_DYLINKER, LC_LOAD_WEAK_DYLIB, LC_MAIN, LC_REEXPORT_DYLIB,
    LC_ROUTINES_64, LC_RPATH, LC_SEGMENT_64, LC_SYMTAB, LC_UUID, MH_BINDS_TO_WEAK, MH_BUNDLE,
    MH_DEAD_STRIPPABLE_DYLIB, MH_DYLDLINK, MH_DYLIB, MH_EXECUTE, MH_HAS_TLV_DESCRIPTORS,
    MH_MAGIC_64, MH_NO_REEXPORTED_DYLIBS, MH_NOUNDEFS, MH_OBJECT, MH_PIE, MH_TWOLEVEL,
    MH_WEAK_DEFINES, S_THREAD_LOCAL_VARIABLES, SECTION_TYPE, TOOL_LD,
};

use super::buf::{align_up, pad_to, push_name16, push32, push64, to_u64};
use super::config::Config;
use super::layout::{Layout, OutSegment};

/// `MAXPATHLEN`, the room `-headerpad_max_install_names` leaves per path.
const MAXPATHLEN: u64 = 1024;

/// A dylib load command.
#[derive(Clone, Debug)]
pub struct DylibLoad {
    /// Command (`LC_LOAD_DYLIB`, `LC_LOAD_WEAK_DYLIB`, `LC_REEXPORT_DYLIB`).
    pub cmd: u32,
    /// Install name.
    pub name: Vec<u8>,
    /// Current version.
    pub current_version: u32,
    /// Compatibility version.
    pub compatibility_version: u32,
}

impl DylibLoad {
    /// The command for a dylib linked with `mode`, all of whose imports are
    /// weak when `all_weak`.
    #[must_use]
    pub fn command(mode: LoadMode, all_weak: bool) -> u32 {
        match mode {
            LoadMode::Reexport => LC_REEXPORT_DYLIB,
            LoadMode::Weak => LC_LOAD_WEAK_DYLIB,
            _ if all_weak => LC_LOAD_WEAK_DYLIB,
            _ => LC_LOAD_DYLIB,
        }
    }
}

/// What the load commands describe, beyond the layout.
#[derive(Clone, Debug, Default)]
pub struct Commands {
    /// Dylib loads, in ordinal order.
    pub dylibs: Vec<DylibLoad>,
    /// `LC_RPATH` paths.
    pub rpaths: Vec<Vec<u8>>,
    /// Whether `LC_FUNCTION_STARTS` is written.
    pub function_starts: bool,
    /// Whether `LC_DATA_IN_CODE` is written.
    pub data_in_code: bool,
    /// The file offset of the entry point (executables).
    pub entry_offset: u64,
    /// `-stack_size`.
    pub stack_size: u64,
    /// Header flags from the contents (`MH_WEAK_DEFINES`, …).
    pub extra_flags: u32,
    /// `MH_NOUNDEFS` applies (nothing is looked up by flat name).
    pub no_undefs: bool,
    /// `-init`: the initializer's address, for `LC_ROUTINES_64`.
    pub init_address: Option<u64>,
}

fn padded_string_size(fixed: u64, text: &[u8]) -> u64 {
    align_up(
        fixed.saturating_add(to_u64(text.len())).saturating_add(1),
        8,
    )
}

/// The size of all load commands.
#[must_use]
pub fn commands_size(config: &Config, layout: &Layout, commands: &Commands) -> (u32, u64) {
    let mut count = 0u32;
    let mut size = 0u64;
    let mut add = |bytes: u64| {
        count = count.saturating_add(1);
        size = size.saturating_add(bytes);
    };
    for segment in &layout.segments {
        add(72u64.saturating_add(80u64.saturating_mul(to_u64(segment.sections.len()))));
    }
    if config.chained_fixups {
        add(16);
        add(16);
    } else {
        add(48);
    }
    add(24);
    add(80);
    for rpath in &commands.rpaths {
        add(padded_string_size(12, rpath));
    }
    match config.output_type {
        MachOutputType::Execute => add(padded_string_size(12, b"/usr/lib/dyld")),
        MachOutputType::Dylib => add(padded_string_size(24, &config.install_name)),
        MachOutputType::Bundle | MachOutputType::Object => {}
    }
    if commands.init_address.is_some() {
        add(72);
    }
    if config.uuid {
        add(24);
    }
    add(32);
    if config.is_exec() {
        add(24);
    }
    for dylib in &commands.dylibs {
        add(padded_string_size(24, &dylib.name));
    }
    if commands.function_starts {
        add(16);
    }
    if commands.data_in_code {
        add(16);
    }
    if config.sign {
        add(16);
    }
    (count, size)
}

/// The header padding: `-headerpad`, or room for every install name with
/// `-headerpad_max_install_names`.
#[must_use]
pub fn headerpad(config: &Config, commands: &Commands) -> u64 {
    let mut pad = config.headerpad;
    if config.headerpad_max_install_names {
        let mut paths = to_u64(commands.dylibs.len());
        if config.output_type == MachOutputType::Dylib {
            paths = paths.saturating_add(1);
        }
        pad = pad.max(paths.saturating_mul(MAXPATHLEN));
    }
    pad
}

/// The `__LINKEDIT` blobs.
#[derive(Clone, Debug, Default)]
pub struct Linkedit {
    /// Chained fixups (or empty).
    pub chained_fixups: Vec<u8>,
    /// Rebase opcodes (legacy).
    pub rebase: Vec<u8>,
    /// Bind opcodes (legacy).
    pub bind: Vec<u8>,
    /// Export trie.
    pub exports: Vec<u8>,
    /// Function starts.
    pub function_starts: Vec<u8>,
    /// Data in code.
    pub data_in_code: Vec<u8>,
    /// Symbol table records.
    pub symbols: Vec<u8>,
    /// Symbol counts: total, locals, extdefs, undefs.
    pub symbol_counts: (u32, u32, u32, u32),
    /// Indirect symbol table.
    pub indirect: Vec<u8>,
    /// Indirect symbol count.
    pub indirect_count: u32,
    /// String table.
    pub strings: Vec<u8>,
}

/// File offsets of the `__LINKEDIT` blobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct LinkeditOffsets {
    chained_fixups: u64,
    rebase: u64,
    bind: u64,
    exports: u64,
    function_starts: u64,
    data_in_code: u64,
    symbols: u64,
    indirect: u64,
    strings: u64,
    /// Where the code signature starts (the end of the other blobs, aligned
    /// to 16).
    pub signature: u64,
}

impl Linkedit {
    /// Lays out the blobs from `start` and returns their offsets.
    #[must_use]
    pub fn offsets(&self, start: u64, config: &Config) -> LinkeditOffsets {
        let mut at = start;
        let mut next = |len: usize| {
            let offset = at;
            at = at.saturating_add(to_u64(len));
            offset
        };
        let mut offsets = LinkeditOffsets::default();
        if config.chained_fixups {
            offsets.chained_fixups = next(self.chained_fixups.len());
            offsets.exports = next(self.exports.len());
        } else {
            offsets.rebase = next(self.rebase.len());
            offsets.bind = next(self.bind.len());
            offsets.exports = next(self.exports.len());
        }
        offsets.function_starts = next(self.function_starts.len());
        offsets.data_in_code = next(self.data_in_code.len());
        offsets.symbols = next(self.symbols.len());
        offsets.indirect = next(self.indirect.len());
        offsets.strings = next(self.strings.len());
        offsets.signature = align_up(at, 16);
        offsets
    }

    /// Copies the blobs into `image` at `offsets`.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if the image is too short.
    pub fn copy_into(&self, image: &mut [u8], offsets: &LinkeditOffsets) -> Result<()> {
        let blobs: [(&[u8], u64); 9] = [
            (&self.chained_fixups, offsets.chained_fixups),
            (&self.rebase, offsets.rebase),
            (&self.bind, offsets.bind),
            (&self.exports, offsets.exports),
            (&self.function_starts, offsets.function_starts),
            (&self.data_in_code, offsets.data_in_code),
            (&self.symbols, offsets.symbols),
            (&self.indirect, offsets.indirect),
            (&self.strings, offsets.strings),
        ];
        for (blob, offset) in blobs {
            if blob.is_empty() {
                continue;
            }
            let start = usize::try_from(offset).map_err(|_| too_short())?;
            let end = start.checked_add(blob.len()).ok_or_else(too_short)?;
            image
                .get_mut(start..end)
                .ok_or_else(too_short)?
                .copy_from_slice(blob);
        }
        Ok(())
    }
}

fn too_short() -> Error {
    Error::Internal("the output is too short for __LINKEDIT".into())
}

fn push_segment(out: &mut Vec<u8>, segment: &OutSegment, layout: &Layout) {
    push32(out, LC_SEGMENT_64);
    push32(
        out,
        u32::try_from(72usize.saturating_add(segment.sections.len().saturating_mul(80)))
            .unwrap_or(0),
    );
    push_name16(out, &segment.name);
    push64(out, segment.vmaddr);
    push64(out, segment.vmsize);
    push64(out, segment.fileoff);
    push64(out, segment.filesize);
    push32(out, segment.maxprot);
    push32(out, segment.initprot);
    push32(out, u32::try_from(segment.sections.len()).unwrap_or(0));
    push32(out, segment.flags);
    for &index in &segment.sections {
        let Some(section) = layout.sections.get(index) else {
            continue;
        };
        push_name16(out, &section.sectname);
        push_name16(out, &section.segname);
        push64(out, section.addr);
        push64(out, section.size);
        push32(out, u32::try_from(section.offset).unwrap_or(0));
        push32(out, section.align);
        push32(out, 0);
        push32(out, 0);
        push32(out, section.flags);
        push32(out, section.reserved1);
        push32(out, section.reserved2);
        push32(out, 0);
    }
}

fn push_string_command(out: &mut Vec<u8>, cmd: u32, fixed: &[u8], text: &[u8]) {
    let size = padded_string_size(to_u64(fixed.len()).saturating_add(8), text);
    let start = out.len();
    push32(out, cmd);
    push32(out, u32::try_from(size).unwrap_or(0));
    out.extend_from_slice(fixed);
    out.extend_from_slice(text);
    out.push(0);
    out.resize(start.saturating_add(usize::try_from(size).unwrap_or(0)), 0);
}

fn push_dylib(out: &mut Vec<u8>, dylib: &DylibLoad) {
    let mut fixed = Vec::new();
    push32(&mut fixed, 24);
    push32(&mut fixed, 0);
    push32(&mut fixed, dylib.current_version);
    push32(&mut fixed, dylib.compatibility_version);
    push_string_command(out, dylib.cmd, &fixed, &dylib.name);
}

/// Packs the qld version for `LC_BUILD_VERSION`'s tool entry.
fn tool_version() -> u32 {
    let mut parts = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|p| p.parse::<u32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0).min(0xffff);
    let minor = parts.next().unwrap_or(0).min(0xff);
    let patch = parts.next().unwrap_or(0).min(0xff);
    (major << 16) | (minor << 8) | patch
}

/// Everything the header needs.
#[derive(Clone, Copy, Debug)]
pub struct HeaderInput<'x> {
    /// Configuration.
    pub config: &'x Config,
    /// Layout (with `__LINKEDIT` sized).
    pub layout: &'x Layout,
    /// Commands.
    pub commands: &'x Commands,
    /// `__LINKEDIT` blobs.
    pub linkedit: &'x Linkedit,
    /// Their offsets.
    pub offsets: &'x LinkeditOffsets,
    /// Code signature size (0 when unsigned).
    pub signature_size: u64,
}

/// The offset of the `LC_UUID` payload within the header, if any.
///
/// # Errors
///
/// [`Error::Internal`] when the commands do not fit the header size planned.
pub fn write_header(input: &HeaderInput<'_>, image: &mut [u8]) -> Result<Option<usize>> {
    let config = input.config;
    let layout = input.layout;
    let commands = input.commands;
    let linkedit = input.linkedit;
    let offsets = input.offsets;
    let mut out = Vec::new();

    let (cpu_type, cpu_subtype) = if config.arch.cpu_type == CPU_TYPE_ARM64 {
        (CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_ALL)
    } else if config.is_exec() {
        (CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_ALL | CPU_SUBTYPE_LIB64)
    } else {
        (CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_ALL)
    };
    let file_type = match config.output_type {
        MachOutputType::Execute => MH_EXECUTE,
        MachOutputType::Dylib => MH_DYLIB,
        MachOutputType::Bundle => MH_BUNDLE,
        MachOutputType::Object => MH_OBJECT,
    };
    let mut flags = MH_DYLDLINK | commands.extra_flags;
    if !config.flat_namespace {
        flags |= MH_TWOLEVEL;
    }
    if commands.no_undefs {
        flags |= MH_NOUNDEFS;
    }
    if config.is_exec() {
        flags |= MH_PIE;
    }
    if config.output_type == MachOutputType::Dylib
        && !commands.dylibs.iter().any(|d| d.cmd == LC_REEXPORT_DYLIB)
    {
        flags |= MH_NO_REEXPORTED_DYLIBS;
    }
    if layout
        .sections
        .iter()
        .any(|s| s.flags & SECTION_TYPE == S_THREAD_LOCAL_VARIABLES)
    {
        flags |= MH_HAS_TLV_DESCRIPTORS;
    }
    let (ncmds, sizeofcmds) = commands_size(config, layout, commands);

    push32(&mut out, MH_MAGIC_64);
    push32(&mut out, cpu_type);
    push32(&mut out, cpu_subtype);
    push32(&mut out, file_type);
    push32(&mut out, ncmds);
    push32(&mut out, u32::try_from(sizeofcmds).unwrap_or(0));
    push32(&mut out, flags);
    push32(&mut out, 0);

    for segment in &layout.segments {
        push_segment(&mut out, segment, layout);
    }
    let blob = |len: usize| u32::try_from(len).unwrap_or(0);
    let offset = |off: u64, len: usize| {
        if len == 0 {
            0
        } else {
            u32::try_from(off).unwrap_or(0)
        }
    };
    if config.chained_fixups {
        push32(&mut out, LC_DYLD_CHAINED_FIXUPS);
        push32(&mut out, 16);
        push32(&mut out, u32::try_from(offsets.chained_fixups).unwrap_or(0));
        push32(&mut out, blob(linkedit.chained_fixups.len()));
        push32(&mut out, LC_DYLD_EXPORTS_TRIE);
        push32(&mut out, 16);
        push32(&mut out, u32::try_from(offsets.exports).unwrap_or(0));
        push32(&mut out, blob(linkedit.exports.len()));
    } else {
        push32(&mut out, LC_DYLD_INFO_ONLY);
        push32(&mut out, 48);
        push32(&mut out, offset(offsets.rebase, linkedit.rebase.len()));
        push32(&mut out, blob(linkedit.rebase.len()));
        push32(&mut out, offset(offsets.bind, linkedit.bind.len()));
        push32(&mut out, blob(linkedit.bind.len()));
        push32(&mut out, 0);
        push32(&mut out, 0);
        push32(&mut out, 0);
        push32(&mut out, 0);
        push32(&mut out, offset(offsets.exports, linkedit.exports.len()));
        push32(&mut out, blob(linkedit.exports.len()));
    }
    let (total, locals, extdefs, undefs) = linkedit.symbol_counts;
    push32(&mut out, LC_SYMTAB);
    push32(&mut out, 24);
    push32(&mut out, u32::try_from(offsets.symbols).unwrap_or(0));
    push32(&mut out, total);
    push32(&mut out, u32::try_from(offsets.strings).unwrap_or(0));
    push32(&mut out, blob(linkedit.strings.len()));

    push32(&mut out, LC_DYSYMTAB);
    push32(&mut out, 80);
    for value in [
        0,
        locals,
        locals,
        extdefs,
        locals.saturating_add(extdefs),
        undefs,
        0,
        0,
        0,
        0,
        0,
        0,
        if linkedit.indirect_count == 0 {
            0
        } else {
            u32::try_from(offsets.indirect).unwrap_or(0)
        },
        linkedit.indirect_count,
        0,
        0,
        0,
        0,
    ] {
        push32(&mut out, value);
    }

    for rpath in &commands.rpaths {
        push_string_command(&mut out, LC_RPATH, &12u32.to_le_bytes(), rpath);
    }
    match config.output_type {
        MachOutputType::Execute => {
            push_string_command(
                &mut out,
                LC_LOAD_DYLINKER,
                &12u32.to_le_bytes(),
                b"/usr/lib/dyld",
            );
        }
        MachOutputType::Dylib => push_dylib(
            &mut out,
            &DylibLoad {
                cmd: LC_ID_DYLIB,
                name: config.install_name.clone(),
                current_version: config.current_version.0,
                compatibility_version: config.compatibility_version.0,
            },
        ),
        MachOutputType::Bundle | MachOutputType::Object => {}
    }
    if let Some(address) = commands.init_address {
        // init_address, init_module, reserved1..6.
        push32(&mut out, LC_ROUTINES_64);
        push32(&mut out, 72);
        push64(&mut out, address);
        for _ in 0..7 {
            push64(&mut out, 0);
        }
    }
    let mut uuid_at = None;
    if config.uuid {
        push32(&mut out, LC_UUID);
        push32(&mut out, 24);
        uuid_at = Some(out.len());
        out.extend_from_slice(&[0; 16]);
    }
    push32(&mut out, LC_BUILD_VERSION);
    push32(&mut out, 32);
    push32(&mut out, config.platform.platform);
    push32(&mut out, config.platform.min.0);
    push32(&mut out, config.platform.sdk.0);
    push32(&mut out, 1);
    push32(&mut out, TOOL_LD);
    push32(&mut out, tool_version());
    if config.is_exec() {
        push32(&mut out, LC_MAIN);
        push32(&mut out, 24);
        push64(&mut out, commands.entry_offset);
        push64(&mut out, commands.stack_size);
    }
    for dylib in &commands.dylibs {
        push_dylib(&mut out, dylib);
    }
    if commands.function_starts {
        push32(&mut out, LC_FUNCTION_STARTS);
        push32(&mut out, 16);
        push32(
            &mut out,
            u32::try_from(offsets.function_starts).unwrap_or(0),
        );
        push32(&mut out, blob(linkedit.function_starts.len()));
    }
    if commands.data_in_code {
        push32(&mut out, LC_DATA_IN_CODE);
        push32(&mut out, 16);
        push32(&mut out, u32::try_from(offsets.data_in_code).unwrap_or(0));
        push32(&mut out, blob(linkedit.data_in_code.len()));
    }
    if config.sign {
        push32(&mut out, LC_CODE_SIGNATURE);
        push32(&mut out, 16);
        push32(&mut out, u32::try_from(offsets.signature).unwrap_or(0));
        push32(&mut out, u32::try_from(input.signature_size).unwrap_or(0));
    }
    let expected = 32u64.saturating_add(sizeofcmds);
    if to_u64(out.len()) != expected || expected > layout.header_size {
        return Err(Error::Internal(format!(
            "load commands take {} bytes, {expected} planned (header area {})",
            out.len(),
            layout.header_size
        )));
    }
    image
        .get_mut(..out.len())
        .ok_or_else(too_short)?
        .copy_from_slice(&out);
    let _ = (MH_WEAK_DEFINES, MH_BINDS_TO_WEAK, MH_DEAD_STRIPPABLE_DYLIB);
    pad_to(&mut out, 1);
    Ok(uuid_at)
}
