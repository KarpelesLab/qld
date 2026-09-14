//! Zstandard compression (RFC 8878) for `--compress-debug-sections=zstd`.
//!
//! # Format choices
//!
//! - The input is cut into [`DEFAULT_CHUNK_SIZE`] chunks compressed in
//!   parallel, each as an independent frame with its content size and no
//!   checksum. Frames concatenate into a valid stream (every decoder,
//!   including `ZSTD_decompress`, reads frames back to back), so the output
//!   depends only on the input and the chunk size, not on the thread count.
//! - Matches are found greedily with one hash-table probe on a five-byte
//!   prefix plus a check of the most recent offset, extended backwards, and
//!   the probe step grows over incompressible data (zstd's "fast"
//!   strategy).
//! - Each 128 KiB block is written as compressed, RLE or raw, whichever is
//!   smallest. Literals are Huffman-coded (weights described directly or
//!   FSE-compressed) when that pays off. Sequences are FSE-coded with the
//!   predefined tables or with per-block tables (RLE when a single code is
//!   used), whichever encodes smaller. Repeat offsets are used.
//!
//! Like the DEFLATE encoder, this only handles buffers it builds itself;
//! indexing and arithmetic follow from the loop invariants noted in the
//! code, and every input is valid.

#![allow(clippy::arithmetic_side_effects)]

use std::sync::OnceLock;

use rayon::prelude::*;

use super::sequences::{LL_BASE, LL_BITS, LL_DEFAULT, ML_BASE, ML_BITS, ML_DEFAULT, OF_DEFAULT};
use crate::debug::compress::deflate::huffman_lengths;

/// Chunk size used by [`zstd_compress`]: each chunk becomes one frame.
pub const DEFAULT_CHUNK_SIZE: usize = 1 << 21;

const MAGIC: u32 = 0xfd2f_b528;
const BLOCK_SIZE: usize = 128 << 10;
const MIN_MATCH: usize = 4;
const HASH_LOG: u32 = 17;
const HUFFMAN_MAX_BITS: usize = 11;

/// Compresses `data` into Zstandard frames of [`DEFAULT_CHUNK_SIZE`] input
/// bytes each, in parallel.
///
/// ```
/// use qld::debug::compress::{zstd::zstd_compress, zstd_decompress_into};
///
/// let data = b"debug info, debug info, debug info".repeat(50);
/// let z = zstd_compress(&data);
/// let mut out = vec![0; data.len()];
/// zstd_decompress_into(&z, &mut out).unwrap();
/// assert_eq!(out, data);
/// ```
#[must_use]
pub fn zstd_compress(data: &[u8]) -> Vec<u8> {
    zstd_compress_chunked(data, DEFAULT_CHUNK_SIZE)
}

/// Like [`zstd_compress`], with an explicit chunk (frame) size of at least
/// one byte.
#[must_use]
pub fn zstd_compress_chunked(data: &[u8], chunk_size: usize) -> Vec<u8> {
    let chunk_size = chunk_size.max(1);
    let count = data.len().div_ceil(chunk_size).max(1);
    let frames: Vec<Vec<u8>> = (0..count)
        .into_par_iter()
        .map(|i| {
            let start = (i * chunk_size).min(data.len());
            let end = (start + chunk_size).min(data.len());
            compress_frame(&data[start..end])
        })
        .collect();
    let total = frames.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for frame in &frames {
        out.extend_from_slice(frame);
    }
    out
}

/// Compresses one frame.
fn compress_frame(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / 3 + 32);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    // Frame header: single segment (the window is the whole content), with
    // the content size in the smallest field that holds it.
    let size = src.len() as u64;
    if size < 256 {
        out.push(0x20);
        out.push(size as u8);
    } else if size < 256 + 65_536 {
        out.push(0x60);
        out.extend_from_slice(&((size - 256) as u16).to_le_bytes());
    } else if size <= u64::from(u32::MAX) {
        out.push(0xa0);
        out.extend_from_slice(&(size as u32).to_le_bytes());
    } else {
        out.push(0xe0);
        out.extend_from_slice(&size.to_le_bytes());
    }

    if src.is_empty() {
        block_header(&mut out, true, 0, 0);
        return out;
    }
    let mut encoder = FrameEncoder {
        hash: vec![0; 1 << HASH_LOG],
        rep: [1, 4, 8],
        sequences: Vec::new(),
        literals: Vec::new(),
    };
    let mut start = 0;
    while start < src.len() {
        let end = (start + BLOCK_SIZE).min(src.len());
        encoder.block(src, start, end, end == src.len(), &mut out);
        start = end;
    }
    out
}

fn block_header(out: &mut Vec<u8>, last: bool, kind: u32, size: usize) {
    let header = u32::from(last) | (kind << 1) | ((size as u32) << 3);
    out.extend_from_slice(&header.to_le_bytes()[..3]);
}

/// A match: `lit_len` literals, then `match_len` bytes from `offset` back.
#[derive(Clone, Copy)]
struct Sequence {
    lit_len: u32,
    match_len: u32,
    offset: u32,
}

struct FrameEncoder {
    /// Position + 1 of the last occurrence of each hashed prefix.
    hash: Vec<u32>,
    /// Repeat offsets as the decoder sees them after the last compressed
    /// block.
    rep: [u32; 3],
    sequences: Vec<Sequence>,
    literals: Vec<u8>,
}

#[inline(always)]
fn read_u64(src: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(src[at..at + 8].try_into().unwrap_or([0; 8]))
}

#[inline(always)]
fn hash5(word: u64) -> usize {
    ((word << 24).wrapping_mul(889_523_592_379) >> (64 - HASH_LOG)) as usize
}

/// Length of the common prefix of `src[a..]` and `src[b..]`, with `a < b`,
/// not extending past `end`.
#[inline(always)]
fn common_len(src: &[u8], a: usize, b: usize, end: usize) -> usize {
    let max = end - b;
    let mut len = 0;
    while len + 8 <= max {
        let diff = read_u64(src, a + len) ^ read_u64(src, b + len);
        if diff != 0 {
            return len + (diff.trailing_zeros() / 8) as usize;
        }
        len += 8;
    }
    while len < max && src[a + len] == src[b + len] {
        len += 1;
    }
    len
}

impl FrameEncoder {
    fn block(&mut self, src: &[u8], start: usize, end: usize, last: bool, out: &mut Vec<u8>) {
        let raw = &src[start..end];
        if raw.len() > 8 && raw.iter().all(|&b| b == raw[0]) {
            block_header(out, last, 1, raw.len());
            out.push(raw[0]);
            return;
        }
        self.find_sequences(src, start, end);
        let mut body = Vec::with_capacity(raw.len() / 2);
        let mut rep = self.rep;
        encode_literals(&self.literals, &mut body);
        encode_sequences(&self.sequences, &mut rep, &mut body);
        if body.len() < raw.len() {
            block_header(out, last, 2, body.len());
            out.extend_from_slice(&body);
            self.rep = rep;
        } else {
            block_header(out, last, 0, raw.len());
            out.extend_from_slice(raw);
        }
    }

    /// Finds the sequences of block `start..end` of `src` (the frame).
    fn find_sequences(&mut self, src: &[u8], start: usize, end: usize) {
        self.sequences.clear();
        self.literals.clear();
        let mut anchor = start;
        let mut ip = start;
        let mut last_offset = 0usize;
        // Eight bytes of lookahead for word reads; matches stop at `end`.
        let limit = end.saturating_sub(8);
        while ip < limit {
            let word = read_u64(src, ip);
            let h = hash5(word);
            let candidate = self.hash[h] as usize;
            self.hash[h] = (ip + 1) as u32;

            let mut best_len = 0;
            let mut best_offset = 0;
            if last_offset != 0 && last_offset <= ip {
                let from = ip - last_offset;
                if read_u64(src, from) as u32 == word as u32 {
                    best_len = 4 + common_len(src, from + 4, ip + 4, end);
                    best_offset = last_offset;
                }
            }
            if candidate != 0 {
                let from = candidate - 1;
                if read_u64(src, from) as u32 == word as u32 {
                    let len = 4 + common_len(src, from + 4, ip + 4, end);
                    if len > best_len {
                        best_len = len;
                        best_offset = ip - from;
                    }
                }
            }

            if best_len >= MIN_MATCH {
                // Extend backwards over the pending literals.
                while ip > anchor && ip > best_offset && src[ip - 1] == src[ip - 1 - best_offset] {
                    ip -= 1;
                    best_len += 1;
                }
                self.literals.extend_from_slice(&src[anchor..ip]);
                self.sequences.push(Sequence {
                    lit_len: (ip - anchor) as u32,
                    match_len: best_len as u32,
                    offset: best_offset as u32,
                });
                ip += best_len;
                anchor = ip;
                last_offset = best_offset;
                // Index a position inside the match for later matches.
                if ip >= start + 2 && ip - 2 + 8 <= src.len() {
                    let at = ip - 2;
                    self.hash[hash5(read_u64(src, at))] = (at + 1) as u32;
                }
            } else {
                // Probe less often the longer nothing matches.
                ip += 1 + ((ip - anchor) >> 8);
            }
        }
        self.literals.extend_from_slice(&src[anchor..end]);
    }
}

// ---------------------------------------------------------------------------
// Bit output
// ---------------------------------------------------------------------------

/// A forward, least-significant-bit-first bit writer. A backward reader
/// returns the fields in reverse order.
///
/// Hot loops call [`put`](Self::put) for up to 56 bits between calls to
/// [`flush`](Self::flush), which stores whole bytes with one fixed-size
/// copy into a buffer kept at least eight bytes longer than the output.
struct BitWriter {
    buf: Vec<u8>,
    pos: usize,
    acc: u64,
    count: u32,
}

impl BitWriter {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0; capacity + 16],
            pos: 0,
            acc: 0,
            count: 0,
        }
    }

    /// Appends the low `bits` (at most 32) bits of `value` without
    /// flushing; the caller keeps the pending bits at most 64.
    #[inline(always)]
    fn put(&mut self, value: u64, bits: u32) {
        let masked = value & ((1u64 << bits) - 1);
        self.acc |= masked << self.count;
        self.count += bits;
    }

    /// Moves the whole pending bytes to the buffer.
    #[inline(always)]
    fn flush(&mut self) {
        if self.pos + 8 > self.buf.len() {
            self.buf.resize(self.buf.len() * 2 + 16, 0);
        }
        self.buf[self.pos..self.pos + 8].copy_from_slice(&self.acc.to_le_bytes());
        let bytes = self.count >> 3;
        self.pos += bytes as usize;
        self.acc = if bytes == 8 {
            0
        } else {
            self.acc >> (bytes * 8)
        };
        self.count &= 7;
    }

    /// Appends the low `bits` (at most 32) bits of `value`.
    #[inline(always)]
    fn add(&mut self, value: u64, bits: u32) {
        self.put(value, bits);
        if self.count >= 32 {
            self.flush();
        }
    }

    /// Adds the end marker and returns the bytes.
    fn close(mut self) -> Vec<u8> {
        self.add(1, 1);
        self.flush();
        if self.count > 0 {
            self.flush_partial();
        }
        self.buf.truncate(self.pos);
        self.buf
    }

    fn flush_partial(&mut self) {
        if self.pos + 8 > self.buf.len() {
            self.buf.resize(self.buf.len() + 16, 0);
        }
        self.buf[self.pos] = self.acc as u8;
        self.pos += 1;
        self.acc = 0;
        self.count = 0;
    }
}

// ---------------------------------------------------------------------------
// FSE encoding
// ---------------------------------------------------------------------------

/// An FSE compression table (zstd's `FSE_CTable`), or an RLE table that
/// encodes one symbol in no bits.
struct CTable {
    log: u32,
    rle: bool,
    /// Next state for each (symbol, sub-range), sorted by symbol.
    states: Vec<u16>,
    /// Per symbol: (delta_find_state, delta_nb_bits).
    symbols: Vec<(i32, u32)>,
}

impl CTable {
    fn rle() -> Self {
        Self {
            log: 0,
            rle: true,
            states: Vec::new(),
            symbols: Vec::new(),
        }
    }

    /// Builds the table for normalized counts `norm` (as the decoder's
    /// `fse::Table::build` spreads them).
    fn new(norm: &[i16], log: u32) -> Self {
        let size = 1usize << log;
        let mask = size - 1;
        let mut high = size - 1;
        let mut symbol_of_state = vec![0u8; size];
        let mut cumul = vec![0usize; norm.len() + 1];
        for (s, &n) in norm.iter().enumerate() {
            if n == -1 {
                cumul[s + 1] = cumul[s] + 1;
                symbol_of_state[high] = s as u8;
                high = high.saturating_sub(1);
            } else {
                cumul[s + 1] = cumul[s] + n.max(0) as usize;
            }
        }
        let step = (size >> 1) + (size >> 3) + 3;
        let mut position = 0;
        for (s, &n) in norm.iter().enumerate() {
            for _ in 0..n.max(0) {
                symbol_of_state[position] = s as u8;
                position = (position + step) & mask;
                while position > high {
                    position = (position + step) & mask;
                }
            }
        }
        let mut states = vec![0u16; size];
        for (u, &s) in symbol_of_state.iter().enumerate() {
            let slot = &mut cumul[usize::from(s)];
            states[*slot] = (size + u) as u16;
            *slot += 1;
        }
        let mut symbols = vec![(0i32, 0u32); norm.len()];
        let mut total = 0i32;
        for (s, &n) in norm.iter().enumerate() {
            symbols[s] = match n {
                0 => (0, ((log + 1) << 16) - (1 << log)),
                -1 | 1 => {
                    let entry = (total - 1, (log << 16) - (1 << log));
                    total += 1;
                    entry
                }
                n => {
                    let n = i32::from(n);
                    let max_bits_out = log - (31 - ((n - 1) as u32).leading_zeros());
                    let min_state_plus = (n as u32) << max_bits_out;
                    let entry = (total - n, (max_bits_out << 16) - min_state_plus);
                    total += n;
                    entry
                }
            };
        }
        Self {
            log,
            rle: false,
            states,
            symbols,
        }
    }

    /// The state that encodes `symbol` without emitting bits
    /// (`FSE_initCState2`).
    fn init(&self, symbol: u8) -> u32 {
        if self.rle {
            return 0;
        }
        let (find, delta_bits) = self.symbols[usize::from(symbol)];
        let bits_out = (delta_bits + (1 << 15)) >> 16;
        let value = (bits_out << 16).wrapping_sub(delta_bits);
        let index = ((value >> bits_out) as i32 + find) as usize;
        u32::from(self.states[index])
    }

    /// Encodes `symbol` (`FSE_encodeSymbol`).
    #[inline(always)]
    fn encode(&self, w: &mut BitWriter, state: &mut u32, symbol: u8) {
        if self.rle {
            return;
        }
        let (find, delta_bits) = self.symbols[usize::from(symbol)];
        let bits_out = (*state + delta_bits) >> 16;
        w.add(u64::from(*state), bits_out);
        let index = ((*state >> bits_out) as i32 + find) as usize;
        *state = u32::from(self.states[index]);
    }

    /// Writes the final state (`FSE_flushCState`).
    fn flush(&self, w: &mut BitWriter, state: u32) {
        if !self.rle {
            w.add(u64::from(state), self.log);
        }
    }
}

/// Normalizes `counts` (summing to `total`) to `1 << log`, giving every
/// used symbol at least one state.
fn normalize(counts: &[u32], total: u32, log: u32) -> Vec<i16> {
    let size = 1i64 << log;
    let total = i64::from(total.max(1));
    let mut norm: Vec<i64> = counts
        .iter()
        .map(|&c| {
            if c == 0 {
                0
            } else {
                ((i64::from(c) * size + total / 2) / total).max(1)
            }
        })
        .collect();
    let mut sum: i64 = norm.iter().sum();
    // Take from the most over-allocated symbols, give to the most
    // under-allocated ones (measured against the exact share).
    while sum > size {
        let pick = (0..norm.len()).filter(|&s| norm[s] > 1).max_by_key(|&s| {
            (
                norm[s] * total - i64::from(counts[s]) * size,
                usize::MAX - s,
            )
        });
        let Some(s) = pick else { break };
        norm[s] -= 1;
        sum -= 1;
    }
    while sum < size {
        let pick = (0..norm.len())
            .filter(|&s| counts[s] != 0)
            .max_by_key(|&s| {
                (
                    i64::from(counts[s]) * size - norm[s] * total,
                    usize::MAX - s,
                )
            });
        let Some(s) = pick else { break };
        norm[s] += 1;
        sum += 1;
    }
    norm.into_iter().map(|n| n as i16).collect()
}

/// `FSE_optimalTableLog` with zstd's parameters.
fn optimal_log(max_log: u32, count: usize, max_symbol: usize) -> u32 {
    let count = count.max(2) as u32;
    let max_bits_src = (31 - (count - 1).leading_zeros()).saturating_sub(2);
    let min_bits = (32 - count.leading_zeros()).min(32 - (max_symbol as u32).leading_zeros() + 1);
    max_bits_src.min(max_log).max(min_bits).clamp(5, max_log)
}

/// Writes an FSE table description (`FSE_writeNCount`).
fn write_ncount(out: &mut Vec<u8>, norm: &[i16], log: u32) {
    let mut acc: u64 = 0;
    let mut count_bits = 0u32;
    let flush = |acc: &mut u64, count_bits: &mut u32, out: &mut Vec<u8>| {
        while *count_bits >= 8 {
            out.push(*acc as u8);
            *acc >>= 8;
            *count_bits -= 8;
        }
    };
    acc |= u64::from(log - 5) << count_bits;
    count_bits += 4;
    let size = 1i32 << log;
    let mut remaining = size + 1;
    let mut threshold = size;
    let mut nb_bits = log + 1;
    let mut symbol = 0usize;
    let mut previous_zero = false;
    while symbol < norm.len() && remaining > 1 {
        if previous_zero {
            let mut start = symbol;
            while symbol < norm.len() && norm[symbol] == 0 {
                symbol += 1;
            }
            if symbol == norm.len() {
                break;
            }
            while symbol >= start + 3 {
                start += 3;
                acc |= 3 << count_bits;
                count_bits += 2;
                flush(&mut acc, &mut count_bits, out);
            }
            acc |= ((symbol - start) as u64) << count_bits;
            count_bits += 2;
            flush(&mut acc, &mut count_bits, out);
        }
        let mut count = i32::from(norm[symbol]);
        symbol += 1;
        let max = 2 * threshold - 1 - remaining;
        remaining -= count.abs();
        count += 1;
        if count >= threshold {
            count += max;
        }
        acc |= (count as u64) << count_bits;
        count_bits += nb_bits;
        if count < max {
            count_bits -= 1;
        }
        previous_zero = count == 1;
        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }
        flush(&mut acc, &mut count_bits, out);
    }
    while count_bits > 0 {
        out.push(acc as u8);
        acc >>= 8;
        count_bits = count_bits.saturating_sub(8);
    }
}

// ---------------------------------------------------------------------------
// Literals
// ---------------------------------------------------------------------------

fn raw_literals(out: &mut Vec<u8>, kind: u32, literals: &[u8]) {
    let n = literals.len() as u32;
    if n < 32 {
        out.push((kind | (n << 3)) as u8);
    } else if n < 4096 {
        out.extend_from_slice(&((kind | (1 << 2) | (n << 4)) as u16).to_le_bytes());
    } else {
        out.extend_from_slice(&(kind | (3 << 2) | (n << 4)).to_le_bytes()[..3]);
    }
    if kind == 0 {
        out.extend_from_slice(literals);
    } else {
        out.push(literals[0]);
    }
}

fn encode_literals(literals: &[u8], out: &mut Vec<u8>) {
    if literals.len() > 8 && literals.iter().all(|&b| b == literals[0]) {
        raw_literals(out, 1, literals);
        return;
    }
    if literals.len() >= 64
        && let Some(section) = huffman_literals(literals)
        && section.len() + 8 < literals.len()
    {
        out.extend_from_slice(&section);
        return;
    }
    raw_literals(out, 0, literals);
}

/// A Huffman-compressed literals section, or `None` when the literals
/// cannot be described.
fn huffman_literals(literals: &[u8]) -> Option<Vec<u8>> {
    let mut freq = [0u32; 256];
    for &b in literals {
        freq[usize::from(b)] += 1;
    }
    let max_symbol = freq.iter().rposition(|&f| f != 0)?;
    if freq.iter().filter(|&&f| f != 0).count() < 2 {
        return None;
    }
    let mut lens = [0u8; 256];
    huffman_lengths(
        &freq[..=max_symbol],
        HUFFMAN_MAX_BITS,
        &mut lens[..=max_symbol],
    );
    let max_bits = usize::from(*lens.iter().max()?);
    let mut weights = [0u8; 256];
    for s in 0..=max_symbol {
        if lens[s] != 0 {
            weights[s] = (max_bits + 1 - usize::from(lens[s])) as u8;
        }
    }

    // Codes, assigned the way the decoder fills its table.
    let mut rank = [0usize; 13];
    for &w in &weights[..=max_symbol] {
        rank[usize::from(w)] += 1;
    }
    let mut start = [0usize; 13];
    let mut next = 0;
    for w in 1..=max_bits {
        start[w] = next;
        next += rank[w] << (w - 1);
    }
    let mut codes = [(0u32, 0u32); 256];
    for s in 0..=max_symbol {
        let w = usize::from(weights[s]);
        if w != 0 {
            codes[s] = ((start[w] >> (w - 1)) as u32, (max_bits + 1 - w) as u32);
            start[w] += 1 << (w - 1);
        }
    }

    let mut body = describe_weights(&weights[..max_symbol])?;
    let n = literals.len();
    let four = n > 1023;
    let stream = |segment: &[u8]| {
        let mut w = BitWriter::new(segment.len());
        // Literals go in reverse order, four codes of at most 11 bits per
        // flush; `as_rchunks` leaves the odd bytes at the front.
        let (head, quads) = segment.as_rchunks::<4>();
        for quad in quads.iter().rev() {
            for &b in quad.iter().rev() {
                let (code, len) = codes[usize::from(b)];
                w.put(u64::from(code), len);
            }
            w.flush();
        }
        for &b in head.iter().rev() {
            let (code, len) = codes[usize::from(b)];
            w.add(u64::from(code), len);
        }
        w.close()
    };
    if four {
        let segment = n.div_ceil(4);
        let parts: Vec<Vec<u8>> = literals.chunks(segment).map(stream).collect();
        if parts.len() != 4 || parts[..3].iter().any(|p| p.len() > 0xffff) {
            return None;
        }
        for part in &parts[..3] {
            body.extend_from_slice(&(part.len() as u16).to_le_bytes());
        }
        for part in &parts {
            body.extend_from_slice(part);
        }
    } else {
        body.extend_from_slice(&stream(literals));
    }

    let compressed = body.len();
    let (n64, c64) = (n as u64, compressed as u64);
    let mut out = Vec::with_capacity(compressed + 5);
    if !four {
        if compressed > 1023 {
            return None;
        }
        let header = 2 | (n64 << 4) | (c64 << 14);
        out.extend_from_slice(&header.to_le_bytes()[..3]);
    } else {
        let largest = n.max(compressed);
        if largest <= 1023 {
            let header = 2 | (1 << 2) | (n64 << 4) | (c64 << 14);
            out.extend_from_slice(&header.to_le_bytes()[..3]);
        } else if largest <= 16_383 {
            let header = 2 | (2 << 2) | (n64 << 4) | (c64 << 18);
            out.extend_from_slice(&header.to_le_bytes()[..4]);
        } else if largest <= 262_143 {
            let header = 2 | (3 << 2) | (n64 << 4) | (c64 << 22);
            out.extend_from_slice(&header.to_le_bytes()[..5]);
        } else {
            return None;
        }
    }
    out.extend_from_slice(&body);
    Some(out)
}

/// Describes Huffman weights (all symbols but the last): FSE-compressed or
/// four bits each, whichever is smaller and representable.
fn describe_weights(weights: &[u8]) -> Option<Vec<u8>> {
    let direct = (weights.len() <= 128).then(|| {
        let mut out = vec![(127 + weights.len()) as u8];
        for pair in weights.chunks(2) {
            out.push((pair[0] << 4) | pair.get(1).copied().unwrap_or(0));
        }
        out
    });
    let compressed = fse_weights(weights).filter(|c| c.len() < 128).map(|c| {
        let mut out = vec![c.len() as u8];
        out.extend_from_slice(&c);
        out
    });
    match (direct, compressed) {
        (Some(d), Some(c)) => Some(if c.len() < d.len() { c } else { d }),
        (d, c) => d.or(c),
    }
}

/// FSE-compresses Huffman weights with two interleaved states
/// (`FSE_compress_usingCTable` in zstd).
fn fse_weights(weights: &[u8]) -> Option<Vec<u8>> {
    let n = weights.len();
    if n <= 2 {
        return None;
    }
    let mut counts = [0u32; 13];
    for &w in weights {
        counts[usize::from(w)] += 1;
    }
    let max_symbol = counts.iter().rposition(|&c| c != 0)?;
    let largest = *counts.iter().max()?;
    if largest as usize == n || largest == 1 {
        return None;
    }
    let log = 6;
    let norm = normalize(&counts[..=max_symbol], n as u32, log);
    let table = CTable::new(&norm, log);
    let mut out = Vec::new();
    write_ncount(&mut out, &norm, log);

    let mut w = BitWriter::new(n);
    let (mut s1, mut s2, mut index);
    if n % 2 == 1 {
        s1 = table.init(weights[n - 1]);
        s2 = table.init(weights[n - 2]);
        table.encode(&mut w, &mut s1, weights[n - 3]);
        index = n - 3;
    } else {
        s2 = table.init(weights[n - 1]);
        s1 = table.init(weights[n - 2]);
        index = n - 2;
    }
    while index > 0 {
        table.encode(&mut w, &mut s2, weights[index - 1]);
        table.encode(&mut w, &mut s1, weights[index - 2]);
        index -= 2;
    }
    table.flush(&mut w, s2);
    table.flush(&mut w, s1);
    out.extend_from_slice(&w.close());
    Some(out)
}

// ---------------------------------------------------------------------------
// Sequences
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Coded {
    ll: u8,
    ml: u8,
    of: u8,
    ll_extra: u32,
    ml_extra: u32,
    of_extra: u32,
}

fn predefined_tables() -> &'static [CTable; 3] {
    static TABLES: OnceLock<[CTable; 3]> = OnceLock::new();
    TABLES.get_or_init(|| {
        [
            CTable::new(&LL_DEFAULT, 6),
            CTable::new(&OF_DEFAULT, 5),
            CTable::new(&ML_DEFAULT, 6),
        ]
    })
}

fn code_for(bases: &[u32], value: u32) -> usize {
    bases.partition_point(|&b| b <= value).saturating_sub(1)
}

/// Literal length code: a table below 64, then one code per power of two
/// (as zstd's `ZSTD_LLcode`).
#[inline(always)]
fn ll_code(lit_len: u32) -> usize {
    static TABLE: OnceLock<[u8; 64]> = OnceLock::new();
    if lit_len >= 64 {
        return (31 - lit_len.leading_zeros() + 19) as usize;
    }
    let table = TABLE.get_or_init(|| std::array::from_fn(|v| code_for(&LL_BASE, v as u32) as u8));
    usize::from(table[lit_len as usize])
}

/// Match length code for a match of `match_len` (at least 3) bytes (as
/// zstd's `ZSTD_MLcode`).
#[inline(always)]
fn ml_code(match_len: u32) -> usize {
    static TABLE: OnceLock<[u8; 128]> = OnceLock::new();
    let base = match_len - 3;
    if base >= 128 {
        return (31 - base.leading_zeros() + 36) as usize;
    }
    let table =
        TABLE.get_or_init(|| std::array::from_fn(|v| code_for(&ML_BASE, v as u32 + 3) as u8));
    usize::from(table[base as usize])
}

/// The offset value to send for `offset` and how the repeat offsets
/// change, mirroring the decoder.
fn offset_value(rep: &mut [u32; 3], offset: u32, lit_len: u32) -> u32 {
    if lit_len != 0 {
        if offset == rep[0] {
            return 1;
        }
        if offset == rep[1] {
            *rep = [rep[1], rep[0], rep[2]];
            return 2;
        }
        if offset == rep[2] {
            *rep = [rep[2], rep[0], rep[1]];
            return 3;
        }
    } else {
        if offset == rep[1] {
            *rep = [rep[1], rep[0], rep[2]];
            return 1;
        }
        if offset == rep[2] {
            *rep = [rep[2], rep[0], rep[1]];
            return 2;
        }
        if rep[0] > 1 && offset == rep[0] - 1 {
            *rep = [offset, rep[0], rep[1]];
            return 3;
        }
    }
    *rep = [offset, rep[0], rep[1]];
    offset + 3
}

fn encode_sequences(sequences: &[Sequence], rep: &mut [u32; 3], out: &mut Vec<u8>) {
    let n = sequences.len();
    if n < 128 {
        out.push(n as u8);
    } else if n < 0x7f00 {
        out.push(((n >> 8) + 128) as u8);
        out.push(n as u8);
    } else {
        out.push(255);
        out.extend_from_slice(&((n - 0x7f00) as u16).to_le_bytes());
    }
    if n == 0 {
        return;
    }

    let coded: Vec<Coded> = sequences
        .iter()
        .map(|s| {
            let ov = offset_value(rep, s.offset, s.lit_len);
            let of = 31 - ov.leading_zeros();
            let ll = ll_code(s.lit_len);
            let ml = ml_code(s.match_len);
            Coded {
                ll: ll as u8,
                ml: ml as u8,
                of: of as u8,
                ll_extra: s.lit_len - LL_BASE[ll],
                ml_extra: s.match_len - ML_BASE[ml],
                of_extra: ov - (1 << of),
            }
        })
        .collect();

    // Small blocks try the predefined tables, large ones fitted tables
    // (whose description then costs little); in between, both are encoded
    // and the smaller kept.
    let mut best: Vec<u8> = Vec::new();
    if n < 1024 {
        let predefined = predefined_tables();
        best.push(0);
        best.extend_from_slice(&write_sequences(
            &coded,
            [&predefined[0], &predefined[1], &predefined[2]],
        ));
    }
    if n >= 16 {
        let mut modes = 0u8;
        let mut descriptions = Vec::new();
        let mut tables = Vec::with_capacity(3);
        for (kind, max_symbol, max_log) in [(0usize, 35usize, 9u32), (1, 31, 8), (2, 52, 9)] {
            let mut counts = vec![0u32; max_symbol + 1];
            for c in &coded {
                counts[usize::from([c.ll, c.of, c.ml][kind])] += 1;
            }
            let used = counts.iter().rposition(|&c| c != 0).unwrap_or(0);
            let shift = 6 - 2 * kind as u8;
            if counts.iter().filter(|&&c| c != 0).count() == 1 {
                modes |= 1 << shift;
                descriptions.push(used as u8);
                tables.push(CTable::rle());
            } else {
                let log = optimal_log(max_log, n, used);
                let norm = normalize(&counts[..=used], n as u32, log);
                modes |= 2 << shift;
                write_ncount(&mut descriptions, &norm, log);
                tables.push(CTable::new(&norm, log));
            }
        }
        let bits = write_sequences(&coded, [&tables[0], &tables[1], &tables[2]]);
        if best.is_empty() || 1 + descriptions.len() + bits.len() < best.len() {
            best.clear();
            best.push(modes);
            best.extend_from_slice(&descriptions);
            best.extend_from_slice(&bits);
        }
    }
    out.extend_from_slice(&best);
}

/// Encodes the sequence bitstream with tables for literal lengths,
/// offsets and match lengths (`ZSTD_encodeSequences`).
fn write_sequences(coded: &[Coded], [ll, of, ml]: [&CTable; 3]) -> Vec<u8> {
    let mut w = BitWriter::new(coded.len() * 3);
    let Some((last, rest)) = coded.split_last() else {
        return w.close();
    };
    let mut ml_state = ml.init(last.ml);
    let mut of_state = of.init(last.of);
    let mut ll_state = ll.init(last.ll);
    w.add(
        u64::from(last.ll_extra),
        u32::from(LL_BITS[usize::from(last.ll)]),
    );
    w.add(
        u64::from(last.ml_extra),
        u32::from(ML_BITS[usize::from(last.ml)]),
    );
    w.add(u64::from(last.of_extra), u32::from(last.of));
    for c in rest.iter().rev() {
        of.encode(&mut w, &mut of_state, c.of);
        ml.encode(&mut w, &mut ml_state, c.ml);
        ll.encode(&mut w, &mut ll_state, c.ll);
        // At most 31 bits are pending after `encode`; the two lengths add
        // at most 32 more, and the offset (after a flush) at most 31.
        w.put(u64::from(c.ll_extra), u32::from(LL_BITS[usize::from(c.ll)]));
        w.put(u64::from(c.ml_extra), u32::from(ML_BITS[usize::from(c.ml)]));
        w.flush();
        w.put(u64::from(c.of_extra), u32::from(c.of));
        w.flush();
    }
    ml.flush(&mut w, ml_state);
    of.flush(&mut w, of_state);
    ll.flush(&mut w, ll_state);
    w.close()
}

#[cfg(test)]
mod tests {
    use super::super::zstd_decompress_into;
    use super::*;
    use crate::debug::compress::inflate::tests::{noise, texty};

    fn roundtrip(data: &[u8], chunk: usize) -> Vec<u8> {
        let z = zstd_compress_chunked(data, chunk);
        let mut out = vec![0u8; data.len()];
        zstd_decompress_into(&z, &mut out).unwrap_or_else(|e| panic!("chunk {chunk}: {e}"));
        assert!(out == data, "chunk {chunk}: output differs");
        z
    }

    #[test]
    fn roundtrips() {
        let sets: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![1],
            b"hello".to_vec(),
            vec![0; 300_000],
            noise(200_000, 3),
            texty(400_000, 5),
            (0..300_000u32).map(|i| (i % 7) as u8 * 31).collect(),
            (0..200_000u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 & 0x3f)
                .collect(),
        ];
        for data in &sets {
            for chunk in [1000, 100_000, DEFAULT_CHUNK_SIZE] {
                roundtrip(data, chunk);
            }
        }
    }

    #[test]
    fn compresses() {
        let data = texty(500_000, 9);
        let z = roundtrip(&data, DEFAULT_CHUNK_SIZE);
        assert!(z.len() < data.len() / 4, "{}", z.len());
        let random = noise(300_000, 1);
        let z = roundtrip(&random, DEFAULT_CHUNK_SIZE);
        assert!(z.len() < random.len() + 64);
    }

    #[test]
    fn length_codes_match_the_tables() {
        // Every length a 128 KiB block can hold.
        for v in 0..131_072u32 {
            assert_eq!(ll_code(v), code_for(&LL_BASE, v), "literal length {v}");
            assert_eq!(
                ml_code(v + 3),
                code_for(&ML_BASE, v + 3),
                "match length {}",
                v + 3
            );
        }
    }

    #[test]
    fn deterministic_across_threads() {
        let data = texty(5_000_000, 2);
        let reference = zstd_compress_chunked(&data, 300_000);
        for threads in [1, 3] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            assert!(pool.install(|| zstd_compress_chunked(&data, 300_000)) == reference);
        }
    }

    #[test]
    fn fse_tables_roundtrip_through_the_decoder() {
        use super::super::fse;
        let counts = [30u32, 1, 0, 7, 100, 3, 0, 0, 1];
        let total = counts.iter().sum();
        for log in [5, 6, 9] {
            let norm = normalize(&counts, total, log);
            assert_eq!(
                norm.iter().map(|&n| i32::from(n).abs()).sum::<i32>(),
                1 << log
            );
            let mut bytes = Vec::new();
            write_ncount(&mut bytes, &norm, log);
            let mut decoded = [0i16; 256];
            let (used, read_log, symbols) = fse::read_ncount(&bytes, 255, 9, &mut decoded).unwrap();
            assert_eq!((used, read_log), (bytes.len(), log));
            assert_eq!(&decoded[..symbols], &norm[..symbols]);
            assert!(norm[symbols..].iter().all(|&n| n == 0));
        }
        // Predefined distributions, with their -1 counts.
        let mut bytes = Vec::new();
        write_ncount(&mut bytes, &LL_DEFAULT, 6);
        let mut decoded = [0i16; 256];
        let (_, _, symbols) = fse::read_ncount(&bytes, 35, 9, &mut decoded).unwrap();
        assert_eq!(&decoded[..symbols], &LL_DEFAULT[..]);
    }
}
