//! Line number programs (`.debug_line`, DWARF 2 to 5).

use super::context::{Context, read_relocated};
use super::reader::{DwarfResult, Reader};
use super::unit::{Encoding, Value, read_value, resolve_string};
use crate::elf::read::ElfFormat;

// Standard opcodes.
const DW_LNS_COPY: u8 = 1;
const DW_LNS_ADVANCE_PC: u8 = 2;
const DW_LNS_ADVANCE_LINE: u8 = 3;
const DW_LNS_SET_FILE: u8 = 4;
const DW_LNS_SET_COLUMN: u8 = 5;
const DW_LNS_NEGATE_STMT: u8 = 6;
const DW_LNS_SET_BASIC_BLOCK: u8 = 7;
const DW_LNS_CONST_ADD_PC: u8 = 8;
const DW_LNS_FIXED_ADVANCE_PC: u8 = 9;
const DW_LNS_SET_PROLOGUE_END: u8 = 10;
const DW_LNS_SET_EPILOGUE_BEGIN: u8 = 11;
const DW_LNS_SET_ISA: u8 = 12;

// Extended opcodes.
const DW_LNE_END_SEQUENCE: u8 = 1;
const DW_LNE_SET_ADDRESS: u8 = 2;
const DW_LNE_DEFINE_FILE: u8 = 3;

// Line table entry content types (DWARF 5).
const DW_LNCT_PATH: u64 = 1;
const DW_LNCT_DIRECTORY_INDEX: u64 = 2;

/// An address range of code attributed to one source line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Range {
    pub(super) section: u32,
    pub(super) start: u64,
    pub(super) end: u64,
    /// Index into [`Builder::files`], or `u32::MAX` if the row named no
    /// valid file.
    pub(super) file: u32,
    pub(super) line: u32,
}

/// Collects the ranges of every line program of an object.
#[derive(Default)]
pub(super) struct Builder {
    pub(super) ranges: Vec<Range>,
    pub(super) files: Vec<String>,
    file_ids: std::collections::HashMap<String, u32>,
}

impl Builder {
    fn intern(&mut self, path: String) -> u32 {
        if let Some(&id) = self.file_ids.get(&path) {
            return id;
        }
        let id = u32::try_from(self.files.len()).unwrap_or(u32::MAX);
        self.files.push(path.clone());
        self.file_ids.insert(path, id);
        id
    }
}

#[derive(Clone, Copy)]
struct Row {
    section: Option<u32>,
    address: u64,
    file: u64,
    line: u64,
}

fn is_absolute(path: &[u8]) -> bool {
    path.first() == Some(&b'/')
        || path.first() == Some(&b'\\')
        || (path.len() > 2 && path[1] == b':' && matches!(path[2], b'/' | b'\\'))
}

/// Joins a file name with its directory and the compilation directory,
/// the way GNU tools display it.
pub(super) fn join_path(name: &[u8], dir: Option<&[u8]>, comp_dir: Option<&[u8]>) -> String {
    let mut out = Vec::new();
    let push = |part: &[u8], out: &mut Vec<u8>| {
        if !out.is_empty() && !out.ends_with(b"/") {
            out.push(b'/');
        }
        out.extend_from_slice(part);
    };
    if !is_absolute(name) {
        let dir = dir.filter(|d| !d.is_empty());
        match (dir, comp_dir.filter(|c| !c.is_empty())) {
            (Some(d), _) if is_absolute(d) => push(d, &mut out),
            (Some(d), Some(c)) => {
                push(c, &mut out);
                push(d, &mut out);
            }
            (Some(d), None) => push(d, &mut out),
            (None, Some(c)) => push(c, &mut out),
            (None, None) => {}
        }
    }
    push(name, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses the line program at the reader's position in section `section`,
/// moving past it, and adds its ranges to `builder`.
pub(super) fn parse_program<F: ElfFormat>(
    ctx: &Context<'_, F>,
    section: u32,
    r: &mut Reader<'_>,
    comp_dir: Option<&[u8]>,
    unit_name: Option<&[u8]>,
    builder: &mut Builder,
) -> DwarfResult<()> {
    let (length, offset_size) = r.initial_length()?;
    let mut p = r.sub(length)?;
    let version = p.u16()?;
    if !(2..=5).contains(&version) {
        return Err(p.error("DWARF line table version (unsupported)"));
    }
    let mut address_size = ctx_address_size::<F>();
    if version >= 5 {
        address_size = usize::from(p.u8()?);
        let _segment_selector_size = p.u8()?;
    }
    let header_length = p.uint(offset_size)?;
    let mut h = p.sub(header_length)?;
    let min_inst = u64::from(h.u8()?);
    let max_ops = if version >= 4 {
        u64::from(h.u8()?).max(1)
    } else {
        1
    };
    let _default_is_stmt = h.u8()?;
    let line_base = i64::from(h.i8()?);
    let line_range = h.u8()?;
    if line_range == 0 {
        return Err(h.error("DWARF line table (line_range is 0)"));
    }
    let opcode_base = h.u8()?;
    let opcode_lengths = h.bytes(usize::from(opcode_base.saturating_sub(1)))?;

    let enc = Encoding {
        version,
        offset_size,
        address_size,
    };
    // Resolved paths, indexed by file number (0-based in DWARF 5, 1-based
    // before: index 0 is then unused).
    let mut files: Vec<u32> = Vec::new();
    if version >= 5 {
        let dirs = read_entries(ctx, section, &mut h, enc)?;
        let dir_paths: Vec<Option<Vec<u8>>> = dirs.iter().map(|e| e.path.clone()).collect();
        for entry in read_entries(ctx, section, &mut h, enc)? {
            let Some(name) = entry.path else {
                files.push(u32::MAX);
                continue;
            };
            let dir = usize::try_from(entry.dir)
                .ok()
                .and_then(|d| dir_paths.get(d))
                .and_then(Option::as_deref);
            files.push(builder.intern(join_path(&name, dir, comp_dir)));
        }
    } else {
        let mut dirs: Vec<&[u8]> = Vec::new();
        loop {
            let dir = h.cstr()?;
            if dir.is_empty() {
                break;
            }
            dirs.push(dir);
        }
        files.push(u32::MAX);
        loop {
            let name = h.cstr()?;
            if name.is_empty() {
                break;
            }
            let dir_index = h.uleb()?;
            h.uleb()?; // modification time
            h.uleb()?; // length
            let dir = dir_index
                .checked_sub(1)
                .and_then(|d| usize::try_from(d).ok())
                .and_then(|d| dirs.get(d))
                .copied();
            files.push(builder.intern(join_path(name, dir, comp_dir)));
        }
    }
    let fallback = unit_name.map(|name| builder.intern(join_path(name, None, comp_dir)));

    // The program itself.
    let mut state = Row {
        section: None,
        address: 0,
        file: 1,
        line: 1,
    };
    let mut op_index = 0u64;
    let mut sequence: Vec<Row> = Vec::new();

    let advance = |state: &mut Row, op_index: &mut u64, operations: u64| {
        let total = op_index.wrapping_add(operations);
        state.address = state
            .address
            .wrapping_add(min_inst.wrapping_mul(total.checked_div(max_ops).unwrap_or(0)));
        *op_index = total.checked_rem(max_ops).unwrap_or(0);
    };
    let range = u64::from(line_range);

    while !p.is_empty() {
        let opcode = p.u8()?;
        if opcode >= opcode_base {
            let adjusted = u64::from(opcode.wrapping_sub(opcode_base));
            advance(
                &mut state,
                &mut op_index,
                adjusted.checked_div(range).unwrap_or(0),
            );
            let delta = line_base.wrapping_add(adjusted.checked_rem(range).unwrap_or(0) as i64);
            state.line = state.line.wrapping_add(delta as u64);
            sequence.push(state);
            continue;
        }
        match opcode {
            0 => {
                let len = p.uleb()?;
                let start = p.pos();
                if len == 0 {
                    continue;
                }
                let sub = p.u8()?;
                match sub {
                    DW_LNE_END_SEQUENCE => {
                        sequence.push(state);
                        finish_sequence(&sequence, &files, fallback, builder);
                        sequence.clear();
                        state = Row {
                            section: None,
                            address: 0,
                            file: 1,
                            line: 1,
                        };
                        op_index = 0;
                    }
                    DW_LNE_SET_ADDRESS => {
                        let size = usize::try_from(len.wrapping_sub(1)).unwrap_or(0);
                        let (address, target) = read_relocated(ctx, section, &mut p, size)?;
                        state.address = address;
                        state.section = target;
                        op_index = 0;
                    }
                    DW_LNE_DEFINE_FILE if version < 5 => {
                        let name = p.cstr()?;
                        p.uleb()?;
                        p.uleb()?;
                        p.uleb()?;
                        files.push(builder.intern(join_path(name, None, comp_dir)));
                    }
                    _ => {}
                }
                // Skip whatever the operands did not consume.
                let end = start
                    .checked_add(usize::try_from(len).unwrap_or(usize::MAX))
                    .ok_or_else(|| p.error("DWARF extended opcode length"))?;
                let rest = end.checked_sub(p.pos()).ok_or_else(|| {
                    p.error("DWARF extended opcode (operands longer than its length)")
                })?;
                p.skip(rest as u64)?;
            }
            DW_LNS_COPY => sequence.push(state),
            DW_LNS_ADVANCE_PC => {
                let pos = p.pos();
                let raw = p.uleb()?;
                let (operations, _) = ctx.relocate(section, pos, raw);
                advance(&mut state, &mut op_index, operations);
            }
            DW_LNS_ADVANCE_LINE => {
                let delta = p.sleb()?;
                state.line = state.line.wrapping_add(delta as u64);
            }
            DW_LNS_SET_FILE => state.file = p.uleb()?,
            DW_LNS_SET_COLUMN | DW_LNS_SET_ISA => {
                p.uleb()?;
            }
            DW_LNS_NEGATE_STMT
            | DW_LNS_SET_BASIC_BLOCK
            | DW_LNS_SET_PROLOGUE_END
            | DW_LNS_SET_EPILOGUE_BEGIN => {}
            DW_LNS_CONST_ADD_PC => {
                let adjusted = u64::from(255u8.wrapping_sub(opcode_base));
                advance(
                    &mut state,
                    &mut op_index,
                    adjusted.checked_div(range).unwrap_or(0),
                );
            }
            DW_LNS_FIXED_ADVANCE_PC => {
                let (delta, _) = read_relocated(ctx, section, &mut p, 2)?;
                state.address = state.address.wrapping_add(delta);
                op_index = 0;
            }
            _ => {
                // An opcode this reader does not know: skip its operands.
                let count = opcode_lengths
                    .get(usize::from(opcode).wrapping_sub(1))
                    .copied()
                    .unwrap_or(0);
                for _ in 0..count {
                    p.uleb()?;
                }
            }
        }
    }
    Ok(())
}

/// The address size to assume for pre-DWARF 5 line tables, whose header
/// does not record it.
fn ctx_address_size<F: ElfFormat>() -> usize {
    F::WORD_SIZE
}

/// A DWARF 5 directory or file entry.
struct Entry {
    path: Option<Vec<u8>>,
    dir: u64,
}

/// Reads a DWARF 5 entry-format description and the entries that follow.
fn read_entries<F: ElfFormat>(
    ctx: &Context<'_, F>,
    section: u32,
    h: &mut Reader<'_>,
    enc: Encoding,
) -> DwarfResult<Vec<Entry>> {
    let format_count = h.u8()?;
    let mut formats = Vec::with_capacity(usize::from(format_count));
    for _ in 0..format_count {
        formats.push((h.uleb()?, h.uleb()?));
    }
    let count = h.uleb()?;
    if formats.is_empty() && count != 0 {
        return Err(h.error("DWARF line table entries (no entry format)"));
    }
    let mut entries = Vec::new();
    for _ in 0..count {
        // Every entry consumes at least one byte, so a bogus count runs out
        // of data rather than memory.
        if h.is_empty() {
            return Err(h.error("DWARF line table entries (truncated)"));
        }
        let mut entry = Entry { path: None, dir: 0 };
        for &(content, form) in &formats {
            let value = read_value(ctx, section, h, form, 0, enc)?;
            match content {
                DW_LNCT_PATH => {
                    entry.path =
                        resolve_string(ctx, value, None, enc.offset_size).map(<[u8]>::to_vec);
                }
                DW_LNCT_DIRECTORY_INDEX => {
                    if let Value::Unsigned(dir, _) = value {
                        entry.dir = dir;
                    }
                }
                _ => {}
            }
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// Turns the rows of one sequence into ranges.
fn finish_sequence(rows: &[Row], files: &[u32], fallback: Option<u32>, builder: &mut Builder) {
    for pair in rows.windows(2) {
        let [a, b] = pair else { continue };
        let (Some(section), Some(b_section)) = (a.section, b.section) else {
            continue;
        };
        if section != b_section || b.address <= a.address {
            continue;
        }
        let file = usize::try_from(a.file)
            .ok()
            .and_then(|f| files.get(f))
            .copied()
            .filter(|&f| f != u32::MAX)
            .or(fallback)
            .unwrap_or(u32::MAX);
        builder.ranges.push(Range {
            section,
            start: a.address,
            end: b.address,
            file,
            line: u32::try_from(a.line).unwrap_or(0),
        });
    }
}
