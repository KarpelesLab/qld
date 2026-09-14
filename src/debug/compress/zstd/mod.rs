//! Zstandard decompression (RFC 8878), for `ELFCOMPRESS_ZSTD` sections.
//!
//! Supported: any number of frames (and skippable frames) in a row; raw,
//! RLE and compressed blocks; raw, RLE, Huffman-compressed and treeless
//! literals, with one or four streams; predefined, RLE, FSE-compressed and
//! repeated sequence tables; repeat offsets; and the optional xxHash64
//! content checksum. Frames that need a dictionary are rejected.
//!
//! Like the zlib decoder, it writes into a buffer of exactly the expected
//! size. Because the whole output is one buffer, the window is simply
//! everything decoded so far in the current frame.
//!
//! Layout: `bits` (bit readers), `fse` (FSE tables), `huffman`
//! (literals), `sequences` (sequence decoding and execution).

mod bits;
mod fse;
mod huffman;
mod sequences;

use super::DecodeError;
use crate::output::hash::xxh64;

const MAGIC: u32 = 0xfd2f_b528;
const SKIPPABLE_MASK: u32 = 0xffff_fff0;
const SKIPPABLE_MAGIC: u32 = 0x184d_2a50;
/// Largest decompressed block (`Block_Maximum_Size`).
const BLOCK_MAX: usize = 128 << 10;
/// Zero bytes after the decoded literals, so that short literal runs can be
/// copied with a fixed-size move.
const LITERAL_PADDING: usize = 16;

/// Decompresses Zstandard data into `out`, which must come out exactly
/// full.
///
/// The input is a sequence of frames (skippable frames are skipped) with
/// nothing after the last one.
///
/// # Errors
///
/// Returns a [`DecodeError`] for corrupt or truncated data, a dictionary
/// frame, a checksum or frame-size mismatch, or a total size other than
/// `out.len()`.
pub fn zstd_decompress_into(input: &[u8], out: &mut [u8]) -> Result<(), DecodeError> {
    let mut at = 0usize;
    let mut o = 0usize;
    let mut decoder = Decoder::new();
    while at < input.len() {
        let magic = read_u32(input, at).ok_or(DecodeError::new(at, "zstd frame (truncated)"))?;
        if magic & SKIPPABLE_MASK == SKIPPABLE_MAGIC {
            let size = read_u32(input, at.wrapping_add(4))
                .ok_or(DecodeError::new(at, "zstd skippable frame (truncated)"))?;
            at = at
                .checked_add(8)
                .and_then(|a| a.checked_add(size as usize))
                .filter(|&end| end <= input.len())
                .ok_or(DecodeError::new(at, "zstd skippable frame (truncated)"))?;
            continue;
        }
        if magic != MAGIC {
            return Err(DecodeError::new(at, "zstd frame magic number"));
        }
        (at, o) = decoder.frame(input, at.wrapping_add(4), out, o)?;
    }
    if o != out.len() {
        return Err(DecodeError::new(
            at,
            "compressed data (smaller than the declared size)",
        ));
    }
    Ok(())
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

struct Decoder {
    huffman: Box<huffman::Table>,
    sequences: sequences::State,
    literals: Vec<u8>,
}

impl Decoder {
    fn new() -> Self {
        Self {
            huffman: Box::new(huffman::Table::new()),
            sequences: sequences::State::new(),
            literals: Vec::new(),
        }
    }

    /// Decodes one frame whose header starts at `at` (after the magic
    /// number). Returns the input and output positions after it.
    fn frame(
        &mut self,
        input: &[u8],
        at: usize,
        out: &mut [u8],
        mut o: usize,
    ) -> Result<(usize, usize), DecodeError> {
        let truncated = DecodeError::new(at, "zstd frame header (truncated)");
        let &descriptor = input.get(at).ok_or(truncated)?;
        let fcs_flag = descriptor >> 6;
        let single_segment = descriptor & 0x20 != 0;
        let has_checksum = descriptor & 0x04 != 0;
        if descriptor & 0x08 != 0 {
            return Err(DecodeError::new(at, "zstd frame header (reserved bit)"));
        }
        let dict_size = [0usize, 1, 2, 4][usize::from(descriptor & 3)];
        let fcs_size = match fcs_flag {
            0 => usize::from(single_segment),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let mut pos = at.wrapping_add(1);
        if !single_segment {
            pos = pos.wrapping_add(1); // Window_Descriptor
        }
        let dict = input
            .get(pos..pos.wrapping_add(dict_size))
            .ok_or(truncated)?;
        if dict.iter().any(|&b| b != 0) {
            return Err(DecodeError::new(
                pos,
                "zstd frame (dictionary not supported)",
            ));
        }
        pos = pos.wrapping_add(dict_size);
        let fcs_bytes = input
            .get(pos..pos.wrapping_add(fcs_size))
            .ok_or(truncated)?;
        let mut content_size = None;
        if fcs_size > 0 {
            let mut buf = [0u8; 8];
            buf.get_mut(..fcs_size)
                .ok_or(truncated)?
                .copy_from_slice(fcs_bytes);
            let mut size = u64::from_le_bytes(buf);
            if fcs_size == 2 {
                size = size.wrapping_add(256);
            }
            content_size = Some(size);
        }
        pos = pos.wrapping_add(fcs_size);

        // Per-frame state: repeat offsets and tables do not carry over.
        self.sequences = sequences::State::new();
        *self.huffman = huffman::Table::new();

        let frame_start = o;
        loop {
            let header_at = pos;
            let header = input
                .get(pos..pos.wrapping_add(3))
                .ok_or(DecodeError::new(pos, "zstd block header (truncated)"))?;
            let header = u32::from_le_bytes([header[0], header[1], header[2], 0]);
            pos = pos.wrapping_add(3);
            let last = header & 1 != 0;
            let size = (header >> 3) as usize;
            let block_start = o;
            match (header >> 1) & 3 {
                0 => {
                    let src = input
                        .get(pos..pos.wrapping_add(size))
                        .ok_or(DecodeError::new(pos, "zstd raw block (truncated)"))?;
                    let end = o.wrapping_add(size);
                    out.get_mut(o..end)
                        .ok_or(too_large(pos))?
                        .copy_from_slice(src);
                    o = end;
                    pos = pos.wrapping_add(size);
                }
                1 => {
                    let &byte = input
                        .get(pos)
                        .ok_or(DecodeError::new(pos, "zstd RLE block (truncated)"))?;
                    let end = o.wrapping_add(size);
                    out.get_mut(o..end).ok_or(too_large(pos))?.fill(byte);
                    o = end;
                    pos = pos.wrapping_add(1);
                }
                2 => {
                    if size > BLOCK_MAX {
                        return Err(DecodeError::new(header_at, "zstd block size"));
                    }
                    let block = input
                        .get(pos..pos.wrapping_add(size))
                        .ok_or(DecodeError::new(pos, "zstd compressed block (truncated)"))?;
                    o = self
                        .compressed_block(block, out, o, frame_start)
                        .map_err(|what| DecodeError::new(pos, what))?;
                    pos = pos.wrapping_add(size);
                }
                _ => return Err(DecodeError::new(header_at, "zstd block type")),
            }
            if o.wrapping_sub(block_start) > BLOCK_MAX {
                return Err(DecodeError::new(header_at, "zstd block (too large)"));
            }
            if last {
                break;
            }
        }

        let content = out.get(frame_start..o).unwrap_or_default();
        if let Some(size) = content_size
            && u64::try_from(content.len()).ok() != Some(size)
        {
            return Err(DecodeError::new(at, "zstd frame content size"));
        }
        if has_checksum {
            let expected = read_u32(input, pos)
                .ok_or(DecodeError::new(pos, "zstd content checksum (truncated)"))?;
            if xxh64(content, 0) as u32 != expected {
                return Err(DecodeError::new(pos, "zstd content checksum (mismatch)"));
            }
            pos = pos.wrapping_add(4);
        }
        Ok((pos, o))
    }

    fn compressed_block(
        &mut self,
        block: &[u8],
        out: &mut [u8],
        o: usize,
        frame_start: usize,
    ) -> Result<usize, &'static str> {
        const BAD: &str = "zstd literals section";
        let &b0 = block.first().ok_or(BAD)?;
        let kind = b0 & 3;
        let size_format = (b0 >> 2) & 3;
        let byte = |i: usize| block.get(i).copied().map(u32::from).ok_or(BAD);
        let rest;
        match kind {
            0 | 1 => {
                let (regenerated, header) = match size_format {
                    0 | 2 => (u32::from(b0 >> 3), 1usize),
                    1 => (u32::from(b0 >> 4) | (byte(1)? << 4), 2),
                    _ => (u32::from(b0 >> 4) | (byte(1)? << 4) | (byte(2)? << 12), 3),
                };
                let regenerated = regenerated as usize;
                if regenerated > BLOCK_MAX {
                    return Err(BAD);
                }
                self.literals.clear();
                if kind == 0 {
                    let end = header.wrapping_add(regenerated);
                    self.literals
                        .extend_from_slice(block.get(header..end).ok_or(BAD)?);
                    rest = block.get(end..).ok_or(BAD)?;
                } else {
                    let value = *block.get(header).ok_or(BAD)?;
                    self.literals.resize(regenerated, value);
                    rest = block.get(header.wrapping_add(1)..).ok_or(BAD)?;
                }
            }
            _ => {
                let (four, bits, header) = match size_format {
                    0 => (false, 10, 3),
                    1 => (true, 10, 3),
                    2 => (true, 14, 4),
                    _ => (true, 18, 5),
                };
                let mut value = 0u64;
                for i in 0..header {
                    value |= u64::from(byte(i)?) << (i.wrapping_mul(8));
                }
                let mask = !u64::MAX.wrapping_shl(bits);
                let regenerated = ((value >> 4) & mask) as usize;
                let compressed = ((value >> bits.wrapping_add(4)) & mask) as usize;
                if regenerated > BLOCK_MAX {
                    return Err(BAD);
                }
                let end = header.checked_add(compressed).ok_or(BAD)?;
                let mut data = block.get(header..end).ok_or(BAD)?;
                rest = block.get(end..).ok_or(BAD)?;
                if kind == 2 {
                    let used = self.huffman.read(data)?;
                    data = data.get(used..).ok_or(BAD)?;
                }
                self.literals.clear();
                self.literals.resize(regenerated, 0);
                self.huffman.decode(data, four, &mut self.literals)?;
            }
        }
        let lit_count = self.literals.len();
        self.literals
            .resize(lit_count.wrapping_add(LITERAL_PADDING), 0);
        sequences::execute(
            &mut self.sequences,
            rest,
            &self.literals,
            lit_count,
            out,
            o,
            frame_start,
        )
    }
}

#[cold]
fn too_large(at: usize) -> DecodeError {
    DecodeError::new(at, "zstd data (larger than the declared size)")
}
