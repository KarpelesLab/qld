//! `--separate-debug-file[=FILE]`: debug information in a file of its own,
//! as mold writes it (GNU ld 2.46 has no such option).
//!
//! The output keeps everything but the debug sections: the non-allocated
//! `.debug_*` sections, `.gdb_index`, `.symtab` and `.strtab` go to FILE
//! (by default the output path plus `.dbg`), and the output gains a
//! `.gnu_debuglink` section naming FILE (its file name only) with a CRC-32
//! that GDB checks when it loads FILE.
//!
//! FILE is laid out like the complete output, section for section, with
//! the same section and program headers, so its symbol table and debug
//! information describe the output as linked. Every section that is not
//! debug information becomes `SHT_NOBITS` (it keeps its address and size
//! but no bytes), except notes (the build-id note among them, with the
//! output's build ID) and `.shstrtab`.
//!
//! The CRC stored in `.gnu_debuglink` has to be known when the output is
//! written, and FILE comes after it, so the value is chosen first, as mold
//! does: the CRC-32 of the output's build ID, or of the output itself when
//! it has none; FILE then gets four trailing bytes that give it that CRC
//! ([`crc32_solve`]). Trailing bytes after the section header table are
//! ignored by ELF readers.

#![deny(clippy::arithmetic_side_effects)]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::args::{BuildId, LinkOptions};
use crate::elf::read::consts::{PT_LOAD, PT_PHDR, SHF_ALLOC, SHT_NOBITS, SHT_NOTE};
use crate::error::{Error, Result};
use crate::ids::SectionId;

use super::layout::{self, Layout, Member, Trailer};
use super::place::Placement;
use super::rules::Synthetic;
use super::sections::Sections;

/// The debug file's path, when `--separate-debug-file` is given.
#[must_use]
pub fn debug_path(options: &LinkOptions) -> Option<PathBuf> {
    match &options.separate_debug_file {
        None => None,
        Some(Some(path)) => Some(path.clone()),
        Some(None) => {
            let mut path = options.output_path().into_os_string();
            path.push(".dbg");
            Some(PathBuf::from(path))
        }
    }
}

/// The file name `.gnu_debuglink` records.
fn link_name(path: &Path) -> &[u8] {
    path.file_name()
        .map_or(path.as_os_str(), |n| n)
        .as_encoded_bytes()
}

/// The size of `.gnu_debuglink` for `path`: the name, a NUL, padding to 4
/// bytes, and the CRC.
#[must_use]
pub fn debuglink_size(path: &Path) -> u64 {
    let name = (link_name(path).len() as u64).saturating_add(1);
    name.saturating_add(3).saturating_add(4) & !3
}

/// The contents of `.gnu_debuglink` for `path`, with a zero CRC.
#[must_use]
pub fn debuglink_contents(path: &Path) -> Vec<u8> {
    let mut out = vec![0u8; usize::try_from(debuglink_size(path)).unwrap_or(0)];
    let name = link_name(path);
    if let Some(dest) = out.get_mut(..name.len()) {
        dest.copy_from_slice(name);
    }
    out
}

/// Whether an output section goes to the debug file.
#[must_use]
pub fn is_debug_section(section: &layout::OutSection<'_>) -> bool {
    if section.flags & SHF_ALLOC != 0 {
        return false;
    }
    match section.trailer {
        Trailer::Symtab | Trailer::Strtab => true,
        Trailer::Generated => section.name != b".gnu_debuglink",
        Trailer::Shstrtab | Trailer::Rela(_) => false,
        Trailer::None => section.name.starts_with(b".debug") || section.name_prefix == b".z",
    }
}

/// A copy of `sections` without the input sections of the debug output
/// sections (non-allocated `.debug*`), for laying out the stripped output.
#[must_use]
pub fn stripped_sections(sections: &Sections, placement: &Placement<'_>) -> Sections {
    let mut live = sections.live.clone();
    for (index, slot) in live.iter_mut().enumerate() {
        if !*slot {
            continue;
        }
        let debug = placement
            .output_of(SectionId::new(index))
            .and_then(|o| placement.outputs.get(o as usize))
            .is_some_and(|o| o.flags & SHF_ALLOC == 0 && o.name.starts_with(b".debug"));
        if debug {
            *slot = false;
        }
    }
    Sections {
        base: sections.base.clone(),
        count: sections.count.clone(),
        owner: sections.owner.clone(),
        live,
        kind: sections.kind.clone(),
        fold_into: sections.fold_into.clone(),
    }
}

/// Turns the complete output's layout into the debug file's: sections that
/// are not debug information, notes or `.shstrtab` become `SHT_NOBITS`,
/// and file offsets are assigned again after the ELF and program headers,
/// allocated sections at offsets congruent to their addresses modulo the
/// page size. Program headers keep their addresses and memory sizes and
/// cover only the bytes left in the file, as `objcopy --only-keep-debug`
/// makes them.
pub fn to_debug_file(layout: &mut Layout<'_>) -> Result<()> {
    let headers = layout.phoff.max(layout.kind.ehdr_size()).saturating_add(
        layout
            .kind
            .phdr_size()
            .saturating_mul(layout.segments.len() as u64),
    );
    let page = layout
        .segments
        .iter()
        .filter(|s| s.p_type == PT_LOAD)
        .map(|s| s.align)
        .max()
        .unwrap_or(layout::DEFAULT_PAGE)
        .max(1);
    // The PT_LOAD of each allocated section; the one mapping the headers
    // is where the file starts.
    let load_of = |addr: u64| {
        layout.segments.iter().position(|g| {
            g.p_type == PT_LOAD && addr >= g.vaddr && addr < g.vaddr.saturating_add(g.memsz)
        })
    };
    let loads: Vec<Option<usize>> = layout
        .sections
        .iter()
        .map(|s| {
            (s.flags & SHF_ALLOC != 0)
                .then(|| load_of(s.addr))
                .flatten()
        })
        .collect();
    let mut current = layout
        .segments
        .iter()
        .position(|g| g.p_type == PT_LOAD && g.offset == 0);
    let mut offset = headers;
    for (section, load) in layout.sections.iter_mut().zip(loads) {
        let keep = is_debug_section(section)
            || section.sh_type == SHT_NOTE
            || section.trailer == Trailer::Shstrtab;
        if !keep {
            section.sh_type = SHT_NOBITS;
        }
        if section.sh_type == SHT_NOBITS {
            section.offset = offset;
            continue;
        }
        offset = if section.flags & SHF_ALLOC != 0 {
            // Congruent to the address modulo the page: the next such
            // offset in the segment being filled, else on a fresh page.
            let skew = section.addr.checked_rem(page).unwrap_or(0);
            let fresh = layout::add(layout::align_up(offset, page)?, skew)?;
            if load.is_some() && load == current {
                let earlier = fresh.saturating_sub(page);
                if earlier >= offset { earlier } else { fresh }
            } else {
                current = load;
                fresh
            }
        } else {
            layout::align_up(offset, section.align)?
        };
        section.offset = offset;
        offset = layout::add(offset, section.size)?;
    }
    layout.shoff = layout::align_up(offset, layout.kind.word_size())?;
    layout.file_size = layout::add(
        layout.shoff,
        layout
            .kind
            .shdr_size()
            .saturating_mul((layout.sections.len() as u64).saturating_add(1)),
    )?;
    // Synthetic parts (the build-id note) move with their sections.
    for (kind, _, file_offset, _) in &mut layout.synthetic {
        if let Some(offset) = member_offset(layout.sections.as_slice(), *kind) {
            *file_offset = offset;
        }
    }
    for segment in &mut layout.segments {
        if segment.p_type == PT_PHDR {
            continue;
        }
        let end = segment.vaddr.saturating_add(segment.memsz);
        let inside = |s: &&layout::OutSection<'_>| {
            s.flags & SHF_ALLOC != 0
                && s.addr >= segment.vaddr
                && s.addr < end.max(segment.vaddr.saturating_add(1))
        };
        let with_bytes = || {
            layout
                .sections
                .iter()
                .filter(inside)
                .filter(|s| s.sh_type != SHT_NOBITS)
        };
        // The segment that maps the headers keeps covering them.
        let headers_too = segment.offset == 0 && segment.p_type == PT_LOAD;
        let start = if headers_too {
            Some(0)
        } else {
            with_bytes()
                .next()
                .or_else(|| layout.sections.iter().find(inside))
                .map(|s| {
                    s.offset
                        .saturating_sub(s.addr.saturating_sub(segment.vaddr))
                })
        };
        segment.offset = start.unwrap_or(0);
        let last = with_bytes()
            .map(|s| s.offset.saturating_add(s.size))
            .max()
            .unwrap_or(0);
        let covered = if headers_too { last.max(headers) } else { last };
        segment.filesz = covered
            .saturating_sub(segment.offset)
            .min(segment.memsz.max(headers));
        if !headers_too && with_bytes().next().is_none() {
            segment.filesz = 0;
        }
    }
    layout.warnings.clear();
    Ok(())
}

/// The file offset of synthetic part `kind` in `sections`.
fn member_offset(sections: &[layout::OutSection<'_>], kind: Synthetic) -> Option<u64> {
    sections.iter().find_map(|section| {
        section
            .members
            .iter()
            .find(|p| p.member == Member::Synthetic(kind))
            .map(|p| section.offset.saturating_add(p.offset))
    })
}

/// The file offset of the build ID bytes of a layout, if it has a build-id
/// note.
#[must_use]
pub fn build_id_offset(layout: &Layout<'_>) -> Option<u64> {
    member_offset(&layout.sections, Synthetic::BuildId).map(|o| o.saturating_add(16))
}

/// The size of the build ID `kind` writes.
fn build_id_size(kind: &BuildId) -> usize {
    match kind {
        BuildId::None => 0,
        BuildId::Fast | BuildId::Md5 | BuildId::Uuid => 16,
        BuildId::Hex(bytes) => bytes.len(),
        _ => 20,
    }
}

/// Finishes the output: stores the CRC `.gnu_debuglink` promises (see the
/// module documentation) and returns it with the output's build ID.
///
/// # Errors
///
/// Returns I/O errors.
pub fn finish_output(
    path: &Path,
    layout: &Layout<'_>,
    options: &LinkOptions,
) -> Result<(u32, Option<Vec<u8>>)> {
    let io = |e| Error::io(path, e);
    let mut file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .map_err(io)?;
    let size = build_id_size(&options.build_id);
    let build_id = match build_id_offset(layout) {
        Some(offset) if size > 0 => {
            let mut bytes = vec![0u8; size];
            file.seek(SeekFrom::Start(offset)).map_err(io)?;
            file.read_exact(&mut bytes).map_err(io)?;
            Some(bytes)
        }
        _ => None,
    };
    let crc = match &build_id {
        Some(bytes) => crc32(bytes),
        None => crc32_file(&mut file).map_err(io)?,
    };
    let link = layout
        .sections
        .iter()
        .find(|s| s.trailer == Trailer::Generated && s.name == b".gnu_debuglink")
        .ok_or_else(|| Error::Internal("no .gnu_debuglink section".into()))?;
    let at = link
        .offset
        .checked_add(link.size)
        .and_then(|end| end.checked_sub(4))
        .ok_or_else(|| Error::Internal(".gnu_debuglink too small".into()))?;
    file.seek(SeekFrom::Start(at)).map_err(io)?;
    file.write_all(&crc.to_le_bytes()).map_err(io)?;
    Ok((crc, build_id))
}

/// Finishes the debug file: copies the output's build ID into its note and
/// appends the four bytes that make its CRC-32 `crc`.
///
/// # Errors
///
/// Returns I/O errors.
pub fn finish_debug_file(
    path: &Path,
    layout: &Layout<'_>,
    build_id: Option<&[u8]>,
    crc: u32,
) -> Result<()> {
    let io = |e| Error::io(path, e);
    let mut file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .map_err(io)?;
    if let (Some(bytes), Some(offset)) = (build_id, build_id_offset(layout)) {
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        file.write_all(bytes).map_err(io)?;
    }
    let current = crc32_file(&mut file).map_err(io)?;
    file.seek(SeekFrom::End(0)).map_err(io)?;
    file.write_all(&crc32_solve(current, crc)).map_err(io)?;
    Ok(())
}

// --- CRC-32 (ISO-HDLC, the polynomial of zlib and `.gnu_debuglink`) ---

/// The reflected CRC-32 polynomial.
const POLY: u32 = 0xedb8_8320;

/// Slicing-by-8 tables.
const TABLES: [[u32; 256]; 8] = make_tables();

// Bounded loops over fixed-size tables.
#[allow(clippy::arithmetic_side_effects)]
const fn make_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut t = 1;
    while t < 8 {
        let mut i = 0;
        while i < 256 {
            let previous = tables[t - 1][i];
            tables[t][i] = (previous >> 8) ^ tables[0][(previous & 0xff) as usize];
            i += 1;
        }
        t += 1;
    }
    tables
}

/// Continues CRC-32 `crc` (of the data so far) over `data`.
fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut crc = !crc;
    let (blocks, rest) = data.as_chunks::<8>();
    for block in blocks {
        let low = crc ^ u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        let high = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
        crc = TABLES[7][(low & 0xff) as usize]
            ^ TABLES[6][((low >> 8) & 0xff) as usize]
            ^ TABLES[5][((low >> 16) & 0xff) as usize]
            ^ TABLES[4][(low >> 24) as usize]
            ^ TABLES[3][(high & 0xff) as usize]
            ^ TABLES[2][((high >> 8) & 0xff) as usize]
            ^ TABLES[1][((high >> 16) & 0xff) as usize]
            ^ TABLES[0][(high >> 24) as usize];
    }
    for &byte in rest {
        crc = (crc >> 8) ^ TABLES[0][((crc ^ u32::from(byte)) & 0xff) as usize];
    }
    !crc
}

/// The CRC-32 of `data`.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

/// Multiplies two polynomials modulo the CRC polynomial (bit-reflected).
fn gf2_multiply(a: u32, mut b: u32) -> u32 {
    let mut product = 0u32;
    let mut bit = 1u32 << 31;
    while bit != 0 {
        if a & bit != 0 {
            product ^= b;
        }
        b = if b & 1 != 0 { (b >> 1) ^ POLY } else { b >> 1 };
        bit >>= 1;
    }
    product
}

/// `x^(8 * len)` modulo the CRC polynomial.
fn x_to_the_bytes(len: u64) -> u32 {
    // x^(2^k) for k = 3 (one byte), squared as len's bits are consumed.
    let mut power = 1u32 << (31 - 8); // x^8
    let mut result = 1u32 << 31; // x^0
    let mut len = len;
    while len != 0 {
        if len & 1 != 0 {
            result = gf2_multiply(result, power);
        }
        power = gf2_multiply(power, power);
        len >>= 1;
    }
    result
}

/// The CRC-32 of `a ++ b` from the CRCs of `a` and `b` and `b`'s length
/// (zlib's `crc32_combine`).
#[must_use]
pub fn crc32_combine(a: u32, b: u32, b_len: u64) -> u32 {
    gf2_multiply(x_to_the_bytes(b_len), a) ^ b
}

/// The four bytes that, appended to data whose CRC-32 is `current`, make
/// the CRC-32 `desired`: the CRC register after them must be `!desired`,
/// so the register is run backwards through 32 zero bits (multiplying by
/// x^-32) and the difference from the current register is what to append.
#[must_use]
pub fn crc32_solve(current: u32, desired: u32) -> [u8; 4] {
    let mut register = !desired;
    for _ in 0..32 {
        // One step back: undo the shift and the conditional XOR.
        register = if register & 0x8000_0000 != 0 {
            ((register ^ POLY) << 1) | 1
        } else {
            register << 1
        };
    }
    (register ^ !current).to_le_bytes()
}

/// Bytes read and checksummed per task.
const CRC_BLOCK: usize = 4 << 20;

/// The CRC-32 of a whole file, read in blocks that are checksummed in
/// parallel and combined.
fn crc32_file(file: &mut File) -> std::io::Result<u32> {
    file.seek(SeekFrom::Start(0))?;
    let mut crc = 0u32;
    let batch = rayon::current_num_threads().clamp(1, 16);
    let mut buffers: Vec<Vec<u8>> = (0..batch).map(|_| vec![0u8; CRC_BLOCK]).collect();
    loop {
        let mut filled = Vec::with_capacity(batch);
        for buffer in &mut buffers {
            let mut len = 0usize;
            while len < buffer.len() {
                let Some(rest) = buffer.get_mut(len..) else {
                    break;
                };
                match file.read(rest)? {
                    0 => break,
                    n => len = len.saturating_add(n),
                }
            }
            filled.push(len);
            if len < CRC_BLOCK {
                break;
            }
        }
        let parts: Vec<(u32, u64)> = buffers
            .par_iter()
            .zip(filled.par_iter())
            .map(|(buffer, &len)| {
                let data = buffer.get(..len).unwrap_or_default();
                (crc32(data), len as u64)
            })
            .collect();
        for (part, len) in parts {
            crc = crc32_combine(crc, part, len);
        }
        if filled.last().is_none_or(|&len| len < CRC_BLOCK) {
            return Ok(crc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let (a, b) = data.split_at(377);
        assert_eq!(
            crc32_combine(crc32(a), crc32(b), b.len() as u64),
            crc32(&data)
        );
        assert_eq!(crc32_update(crc32(a), b), crc32(&data));
    }

    #[test]
    fn crc32_solve_forges_the_crc() {
        for (data, desired) in [(&b"hello"[..], 0xdead_beef), (b"", 0), (b"x", 0x1234_5678)] {
            let mut forged = data.to_vec();
            forged.extend_from_slice(&crc32_solve(crc32(data), desired));
            assert_eq!(crc32(&forged), desired);
        }
    }

    #[test]
    fn debuglink_is_padded() {
        assert_eq!(debuglink_size(Path::new("dir/a.dbg")), 12);
        assert_eq!(debuglink_size(Path::new("abc")), 8);
        let contents = debuglink_contents(Path::new("dir/a.dbg"));
        assert_eq!(&contents, b"a.dbg\0\0\0\0\0\0\0");
    }
}
