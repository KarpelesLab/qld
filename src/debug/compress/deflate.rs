//! DEFLATE (RFC 1951) compression, split into chunks that compress in
//! parallel, as lld does for `--compress-debug-sections=zlib`.
//!
//! # Chunked streams
//!
//! [`zlib_compress`] cuts its input into [`DEFAULT_CHUNK_SIZE`] chunks and
//! compresses them independently on the rayon pool. Every chunk but the last
//! ends with a *sync flush*: an empty, non-final stored block that leaves
//! the stream byte-aligned. The raw chunk outputs therefore concatenate into
//! one valid DEFLATE stream. The zlib header goes in front, and the Adler-32
//! trailer is combined from per-chunk checksums with
//! [`super::adler32_combine`]. Chunks never refer to each
//! other's data, so the output depends only on the input, the level and the
//! chunk size, never on the thread count.
//!
//! # Encoder
//!
//! - LZ77 with hash chains over four-byte prefixes (a 2^15-entry head table
//!   and a 32 KiB chain of 16-bit deltas). Levels follow zlib's parameter
//!   table (`good`, `lazy`, `nice`, `chain`): levels 1–3 match greedily and
//!   levels 4–9 use lazy evaluation. Level 0 writes stored blocks.
//! - Symbols are buffered in blocks of [`BLOCK_SYMBOLS`]. Each block is
//!   written as stored, fixed-Huffman or dynamic-Huffman, whichever is
//!   smallest; dynamic codes are optimal Huffman codes (Moffat–Katajainen)
//!   limited to 15 bits (7 for the code-length code).
//!
//! The encoder only handles buffers it built itself, so it is written with
//! plain arithmetic and indexing whose bounds follow from the loop
//! invariants noted in the code; any input bytes are valid.

#![allow(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use super::adler32::{adler32, adler32_combine};

/// Chunk size used by [`zlib_compress`]: 1 MiB, as in lld.
pub const DEFAULT_CHUNK_SIZE: usize = 1 << 20;

/// Symbols per Huffman block (zlib's `lit_bufsize` at the default memory
/// level).
pub const BLOCK_SYMBOLS: usize = 1 << 14;

const WINDOW: usize = 1 << 15;
const HASH_BITS: u32 = 15;
const MIN_MATCH: usize = 4;
const MAX_MATCH: usize = 258;
/// Offset added to positions stored in the head table, so that the zero
/// initial value is always out of the window.
const POS_BIAS: usize = WINDOW + 1;

/// A compression level, 0 (stored) to 9 (smallest), with zlib's meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Level(u8);

impl Level {
    /// No compression: stored blocks only.
    pub const STORE: Self = Self(0);
    /// Fastest compression (zlib level 1). lld's default for debug sections.
    pub const FASTEST: Self = Self(1);
    /// zlib's default trade-off (level 6). lld uses it with `-O2`.
    pub const DEFAULT: Self = Self(6);
    /// Smallest output (level 9).
    pub const BEST: Self = Self(9);

    /// Creates a level, clamping values above 9.
    #[must_use]
    pub const fn new(level: u8) -> Self {
        Self(if level > 9 { 9 } else { level })
    }

    /// The numeric level.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The second byte of the zlib header (`FLG`), with `FLEVEL` set as zlib
    /// sets it for this level and the check bits making the header a
    /// multiple of 31.
    #[must_use]
    pub const fn zlib_flg(self) -> u8 {
        match self.0 {
            0 | 1 => 0x01,
            2..=5 => 0x5e,
            6 => 0x9c,
            _ => 0xda,
        }
    }
}

impl Default for Level {
    fn default() -> Self {
        Self::FASTEST
    }
}

/// zlib's per-level tuning (`configuration_table` in `deflate.c`).
#[derive(Clone, Copy)]
struct Params {
    /// Reduce the chain search when the previous match is this long.
    good: usize,
    /// Greedy levels: only insert the strings of matches up to this long.
    /// Lazy levels: do not look for a better match past this length.
    lazy: usize,
    /// Stop searching when a match is this long.
    nice: usize,
    /// Maximum hash chain steps.
    chain: usize,
    /// Lazy evaluation (levels 4–9).
    lazy_mode: bool,
}

const fn params(good: usize, lazy: usize, nice: usize, chain: usize, lazy_mode: bool) -> Params {
    Params {
        good,
        lazy,
        nice,
        chain,
        lazy_mode,
    }
}

const PARAMS: [Params; 10] = [
    params(0, 0, 0, 0, false),
    params(4, 4, 8, 4, false),
    params(4, 5, 16, 8, false),
    params(4, 6, 32, 32, false),
    params(4, 4, 16, 16, true),
    params(8, 16, 32, 32, true),
    params(8, 16, 128, 128, true),
    params(8, 32, 128, 256, true),
    params(32, 128, 258, 1024, true),
    params(32, 258, 258, 4096, true),
];

/// Compresses `data` into a zlib stream (RFC 1950), using
/// [`DEFAULT_CHUNK_SIZE`] chunks compressed in parallel.
///
/// The result is identical for any number of threads.
///
/// ```
/// use qld::debug::compress::deflate::{Level, zlib_compress};
/// use qld::debug::compress::zlib_decompress_into;
///
/// let data = b"abcabcabcabcabcabcabc".repeat(100);
/// let z = zlib_compress(&data, Level::FASTEST);
/// let mut out = vec![0; data.len()];
/// zlib_decompress_into(&z, &mut out).unwrap();
/// assert_eq!(out, data);
/// ```
#[must_use]
pub fn zlib_compress(data: &[u8], level: Level) -> Vec<u8> {
    zlib_compress_chunked(data, level, DEFAULT_CHUNK_SIZE)
}

/// Like [`zlib_compress`], with an explicit chunk size (at least 1 byte).
#[must_use]
pub fn zlib_compress_chunked(data: &[u8], level: Level, chunk_size: usize) -> Vec<u8> {
    let chunk_size = chunk_size.max(1);
    let count = data.len().div_ceil(chunk_size).max(1);
    let parts: Vec<(Vec<u8>, u32)> = (0..count)
        .into_par_iter()
        .map(|i| {
            let start = (i * chunk_size).min(data.len());
            let end = (start + chunk_size).min(data.len());
            let chunk = &data[start..end];
            (deflate_chunk(chunk, level, i + 1 == count), adler32(chunk))
        })
        .collect();

    let total: usize = parts.iter().map(|(bytes, _)| bytes.len()).sum();
    let mut out = Vec::with_capacity(total + 6);
    out.extend_from_slice(&[0x78, level.zlib_flg()]);
    let mut checksum = super::ADLER32_INIT;
    for (i, (bytes, adler)) in parts.iter().enumerate() {
        out.extend_from_slice(bytes);
        let start = (i * chunk_size).min(data.len());
        let len = (start + chunk_size).min(data.len()) - start;
        checksum = adler32_combine(checksum, *adler, len as u64);
    }
    out.extend_from_slice(&checksum.to_be_bytes());
    out
}

/// Compresses one chunk into raw DEFLATE data.
///
/// With `last`, the data ends with a final block. Otherwise it ends with a
/// sync flush (an empty stored block), so that the next chunk's output can
/// follow it directly.
#[must_use]
pub fn deflate_chunk(data: &[u8], level: Level, last: bool) -> Vec<u8> {
    let mut w = BitWriter::with_capacity(data.len() / 3 + 64);
    if level.0 == 0 {
        write_stored(&mut w, data, last);
    } else if data.is_empty() {
        if last {
            // A final fixed-Huffman block holding only end-of-block.
            w.put(0b011, 3);
            w.put(0, 7);
        }
    } else {
        let mut encoder = Encoder::new(PARAMS[usize::from(level.0)], data.len());
        encoder.run(data, &mut w, last);
    }
    if !last {
        // Sync flush: an empty non-final stored block.
        w.put(0, 3);
        w.align();
        w.out.extend_from_slice(&[0, 0, 0xff, 0xff]);
    }
    w.finish()
}

/// LSB-first bit writer.
struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    count: u32,
}

impl BitWriter {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            out: Vec::with_capacity(capacity),
            buf: 0,
            count: 0,
        }
    }

    /// Writes the low `n` bits of `bits` (`n` ≤ 32, and higher bits clear).
    #[inline(always)]
    fn put(&mut self, bits: u32, n: u32) {
        self.buf |= u64::from(bits) << self.count;
        self.count += n;
        if self.count >= 32 {
            self.out.extend_from_slice(&(self.buf as u32).to_le_bytes());
            self.buf >>= 32;
            self.count -= 32;
        }
    }

    /// Pads with zero bits to a byte boundary and flushes the buffer.
    fn align(&mut self) {
        while self.count > 0 {
            self.out.push(self.buf as u8);
            self.buf >>= 8;
            self.count = self.count.saturating_sub(8);
        }
        self.buf = 0;
    }

    fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }

    /// Bits written so far.
    fn bit_len(&self) -> u64 {
        self.out.len() as u64 * 8 + u64::from(self.count)
    }
}

/// Writes `data` as stored blocks.
fn write_stored(w: &mut BitWriter, data: &[u8], last: bool) {
    let mut pieces = data.chunks(0xffff).peekable();
    if pieces.peek().is_none() {
        if last {
            w.put(1, 3);
            w.align();
            w.out.extend_from_slice(&[0, 0, 0xff, 0xff]);
        }
        return;
    }
    while let Some(piece) = pieces.next() {
        let is_final = last && pieces.peek().is_none();
        w.put(u32::from(is_final), 3);
        w.align();
        let len = piece.len() as u16;
        w.out.extend_from_slice(&len.to_le_bytes());
        w.out.extend_from_slice(&(!len).to_le_bytes());
        w.out.extend_from_slice(piece);
    }
}

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

/// A buffered symbol: a literal (`dist == 0`, `value` = byte) or a match
/// (`value` = length − 3, `dist` = distance).
#[derive(Clone, Copy)]
struct Symbol {
    value: u16,
    dist: u16,
}

/// Literal/length symbol for a match length (3..=258).
#[inline(always)]
fn length_symbol(len: usize) -> (usize, u32, u32) {
    let l = (len - 3) as u32;
    if l < 8 {
        (257 + l as usize, 0, 0)
    } else if l == 255 {
        (285, 0, 0)
    } else {
        let b = 31 - l.leading_zeros(); // 3..=7
        let extra = b - 2;
        let sym = 257 + 4 * (b as usize - 1) + ((l >> extra) & 3) as usize;
        (sym, extra, l & ((1 << extra) - 1))
    }
}

/// Distance symbol for a distance (1..=32768).
#[inline(always)]
fn distance_symbol(dist: usize) -> (usize, u32, u32) {
    let d = (dist - 1) as u32;
    if d < 4 {
        (d as usize, 0, 0)
    } else {
        let b = 31 - d.leading_zeros(); // 2..=14
        let extra = b - 1;
        let sym = 2 * b as usize + ((d >> extra) & 1) as usize;
        (sym, extra, d & ((1 << extra) - 1))
    }
}

// ---------------------------------------------------------------------------
// LZ77
// ---------------------------------------------------------------------------

struct Encoder {
    p: Params,
    /// Most recent position (+ `POS_BIAS`) for each hash.
    head: Vec<u32>,
    /// For position `i`, the distance back to the previous position with
    /// the same hash (0 = none), indexed by `i & WINDOW_MASK`.
    prev: Vec<u16>,
    symbols: Vec<Symbol>,
    /// Input bytes covered by `symbols`, starting at `block_start`.
    block_start: usize,
    covered: usize,
}

#[inline(always)]
fn hash4(data: &[u8], pos: usize) -> usize {
    let word = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
    (word.wrapping_mul(0x9e37_79b1) >> (32 - HASH_BITS)) as usize
}

/// Length of the common prefix of `data[a..]` and `data[b..]`, up to `max`,
/// where `a < b` and `b + max <= data.len()`.
#[inline(always)]
fn match_len(data: &[u8], a: usize, b: usize, max: usize) -> usize {
    let mut len = 0;
    while len + 8 <= max {
        let x = u64::from_le_bytes(data[a + len..a + len + 8].try_into().unwrap_or([0; 8]));
        let y = u64::from_le_bytes(data[b + len..b + len + 8].try_into().unwrap_or([0; 8]));
        let diff = x ^ y;
        if diff != 0 {
            return len + (diff.trailing_zeros() / 8) as usize;
        }
        len += 8;
    }
    while len < max && data[a + len] == data[b + len] {
        len += 1;
    }
    len
}

impl Encoder {
    fn new(p: Params, len: usize) -> Self {
        Self {
            p,
            head: vec![0; 1 << HASH_BITS],
            prev: vec![0; WINDOW.min(len.next_power_of_two())],
            symbols: Vec::with_capacity(BLOCK_SYMBOLS),
            block_start: 0,
            covered: 0,
        }
    }

    /// Inserts position `pos` (which has four bytes of lookahead) into the
    /// hash chains and returns the distance to the previous position with
    /// the same hash, or 0.
    #[inline(always)]
    fn insert(&mut self, data: &[u8], pos: usize) -> usize {
        let h = hash4(data, pos);
        let biased = pos + POS_BIAS;
        let dist = biased - self.head[h] as usize;
        self.head[h] = biased as u32;
        let dist = if dist <= WINDOW { dist } else { 0 };
        let mask = self.prev.len() - 1;
        self.prev[pos & mask] = dist as u16;
        dist
    }

    /// Finds the longest match for `pos`, starting from a candidate `dist`
    /// bytes back and following the chain. Only matches longer than `best`
    /// are reported. Returns `(length, distance)`; the length is at most
    /// `best` if nothing longer was found.
    #[inline(always)]
    fn longest_match(
        &self,
        data: &[u8],
        pos: usize,
        mut dist: usize,
        mut best: usize,
        mut chain: usize,
    ) -> (usize, usize) {
        let max = MAX_MATCH.min(data.len() - pos);
        let nice = self.p.nice.min(max);
        let mask = self.prev.len() - 1;
        let mut best_dist = 0;
        if best >= max {
            return (best, 0);
        }
        while dist != 0 && dist <= WINDOW {
            let cand = pos - dist;
            // Cheap reject: the byte that would extend the best match.
            if data[cand + best] == data[pos + best] {
                let len = match_len(data, cand, pos, max);
                if len > best {
                    best = len;
                    best_dist = dist;
                    if len >= nice {
                        break;
                    }
                }
            }
            chain -= 1;
            if chain == 0 {
                break;
            }
            let step = self.prev[cand & mask] as usize;
            if step == 0 {
                break;
            }
            dist += step;
        }
        (best, best_dist)
    }

    #[inline(always)]
    fn literal(&mut self, byte: u8) {
        self.symbols.push(Symbol {
            value: u16::from(byte),
            dist: 0,
        });
        self.covered += 1;
    }

    #[inline(always)]
    fn matched(&mut self, len: usize, dist: usize) {
        self.symbols.push(Symbol {
            value: (len - 3) as u16,
            dist: dist as u16,
        });
        self.covered += len;
    }

    #[inline(always)]
    fn maybe_flush(&mut self, data: &[u8], w: &mut BitWriter) {
        if self.symbols.len() >= BLOCK_SYMBOLS {
            self.flush(data, w, false);
        }
    }

    fn flush(&mut self, data: &[u8], w: &mut BitWriter, is_final: bool) {
        let end = self.block_start + self.covered;
        write_block(w, &self.symbols, &data[self.block_start..end], is_final);
        self.symbols.clear();
        self.block_start = end;
        self.covered = 0;
    }

    fn run(&mut self, data: &[u8], w: &mut BitWriter, last: bool) {
        if self.p.lazy_mode {
            self.run_lazy(data, w);
        } else {
            self.run_greedy(data, w);
        }
        self.flush(data, w, last);
    }

    /// zlib's `deflate_fast`.
    fn run_greedy(&mut self, data: &[u8], w: &mut BitWriter) {
        let n = data.len();
        let mut pos = 0;
        while pos + MIN_MATCH <= n {
            let dist = self.insert(data, pos);
            let (len, dist) = if dist != 0 {
                self.longest_match(data, pos, dist, MIN_MATCH - 1, self.p.chain)
            } else {
                (0, 0)
            };
            if len >= MIN_MATCH {
                self.matched(len, dist);
                if len <= self.p.lazy {
                    let end = (pos + len).min(n - MIN_MATCH + 1);
                    for q in pos + 1..end {
                        self.insert(data, q);
                    }
                }
                pos += len;
            } else {
                self.literal(data[pos]);
                pos += 1;
            }
            self.maybe_flush(data, w);
        }
        while pos < n {
            self.literal(data[pos]);
            pos += 1;
            self.maybe_flush(data, w);
        }
    }

    /// zlib's `deflate_slow`.
    fn run_lazy(&mut self, data: &[u8], w: &mut BitWriter) {
        let n = data.len();
        let mut pos = 0;
        // A match (or a literal, when `prev_len` < MIN_MATCH) found at
        // `pos - 1` and not yet emitted.
        let mut pending = false;
        let mut prev_len = 0;
        let mut prev_dist = 0;
        while pos < n {
            let (mut len, mut dist) = (0, 0);
            if pos + MIN_MATCH <= n {
                let cand = self.insert(data, pos);
                if cand != 0 && prev_len < self.p.lazy {
                    let chain = if prev_len >= self.p.good {
                        (self.p.chain >> 2).max(1)
                    } else {
                        self.p.chain
                    };
                    let floor = prev_len.max(MIN_MATCH - 1);
                    (len, dist) = self.longest_match(data, pos, cand, floor, chain);
                    if dist == 0 {
                        len = 0;
                    }
                }
            }
            if pending && prev_len >= MIN_MATCH && len <= prev_len {
                // The match at pos - 1 is at least as good: emit it.
                self.matched(prev_len, prev_dist);
                let end = (pos - 1 + prev_len).min(n.saturating_sub(MIN_MATCH - 1));
                for q in pos + 1..end {
                    self.insert(data, q);
                }
                pos = pos - 1 + prev_len;
                pending = false;
                prev_len = 0;
            } else {
                if pending {
                    self.literal(data[pos - 1]);
                }
                pending = true;
                prev_len = len;
                prev_dist = dist;
                pos += 1;
            }
            self.maybe_flush(data, w);
        }
        if pending {
            self.literal(data[n - 1]);
        }
    }
}

// ---------------------------------------------------------------------------
// Huffman blocks
// ---------------------------------------------------------------------------

const LITLEN_SYMBOLS: usize = 286;
const DIST_SYMBOLS: usize = 30;
const PRECODE_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn fixed_litlen_len(symbol: usize) -> u8 {
    match symbol {
        0..=143 => 8,
        144..=255 => 9,
        256..=279 => 7,
        _ => 8,
    }
}

/// Writes one block of `symbols` (covering the input bytes `raw`) in the
/// cheapest of the three block types.
fn write_block(w: &mut BitWriter, symbols: &[Symbol], raw: &[u8], is_final: bool) {
    let mut lit_freq = [0u32; LITLEN_SYMBOLS];
    let mut dist_freq = [0u32; DIST_SYMBOLS];
    let mut extra_bits = 0u64;
    for s in symbols {
        if s.dist == 0 {
            lit_freq[usize::from(s.value)] += 1;
        } else {
            let (ls, le, _) = length_symbol(usize::from(s.value) + 3);
            let (ds, de, _) = distance_symbol(usize::from(s.dist));
            lit_freq[ls] += 1;
            dist_freq[ds] += 1;
            extra_bits += u64::from(le + de);
        }
    }
    lit_freq[256] = 1;

    let mut lit_lens = [0u8; LITLEN_SYMBOLS];
    let mut dist_lens = [0u8; DIST_SYMBOLS];
    huffman_lengths(&lit_freq, 15, &mut lit_lens);
    huffman_lengths(&dist_freq, 15, &mut dist_lens);
    let header = DynamicHeader::new(&lit_lens, &dist_lens);

    let data_cost = |lens_lit: &dyn Fn(usize) -> u8, lens_dist: &dyn Fn(usize) -> u8| -> u64 {
        let lit: u64 = lit_freq
            .iter()
            .enumerate()
            .map(|(i, &f)| u64::from(f) * u64::from(lens_lit(i)))
            .sum();
        let dist: u64 = dist_freq
            .iter()
            .enumerate()
            .map(|(i, &f)| u64::from(f) * u64::from(lens_dist(i)))
            .sum();
        lit + dist + extra_bits
    };
    let dynamic_cost = 3 + header.cost() + data_cost(&|i| lit_lens[i], &|i| dist_lens[i]);
    let fixed_cost = 3 + data_cost(&fixed_litlen_len, &|_| 5);
    // Stored: each piece has a 3-bit header, padding to a byte, and 4 bytes
    // of lengths. Only the first padding depends on the current position.
    let pieces = raw.len().div_ceil(0xffff).max(1) as u64;
    let first_pad = u64::from((8 - (w.count + 3) % 8) % 8);
    let stored_cost = first_pad + pieces * (3 + 32) + (pieces - 1) * 5 + raw.len() as u64 * 8;

    if stored_cost < dynamic_cost.min(fixed_cost) {
        write_stored(w, raw, is_final);
        return;
    }
    let final_bit = u32::from(is_final);
    if fixed_cost <= dynamic_cost {
        let mut lit_lens = [0u8; 288];
        for (i, l) in lit_lens.iter_mut().enumerate() {
            *l = fixed_litlen_len(i);
        }
        let dist_lens = [5u8; 30];
        w.put(final_bit | (1 << 1), 3);
        write_symbols(w, symbols, &Code::new(&lit_lens), &Code::new(&dist_lens));
    } else {
        w.put(final_bit | (2 << 1), 3);
        let start = w.bit_len();
        header.write(w);
        debug_assert_eq!(w.bit_len() - start, header.cost());
        write_symbols(w, symbols, &Code::new(&lit_lens), &Code::new(&dist_lens));
    }
}

fn write_symbols(w: &mut BitWriter, symbols: &[Symbol], lit: &Code, dist: &Code) {
    for s in symbols {
        if s.dist == 0 {
            let i = usize::from(s.value);
            w.put(lit.codes[i], u32::from(lit.lens[i]));
        } else {
            let (ls, le, lv) = length_symbol(usize::from(s.value) + 3);
            let (ds, de, dv) = distance_symbol(usize::from(s.dist));
            w.put(
                lit.codes[ls] | (lv << lit.lens[ls]),
                u32::from(lit.lens[ls]) + le,
            );
            w.put(
                dist.codes[ds] | (dv << dist.lens[ds]),
                u32::from(dist.lens[ds]) + de,
            );
        }
    }
    w.put(lit.codes[256], u32::from(lit.lens[256]));
}

/// A canonical Huffman code, with codes bit-reversed for LSB-first output.
struct Code {
    codes: [u32; 288],
    lens: [u8; 288],
}

impl Code {
    fn new(lens: &[u8]) -> Self {
        let mut count = [0u32; 16];
        for &l in lens {
            count[usize::from(l)] += 1;
        }
        count[0] = 0;
        let mut next = [0u32; 16];
        let mut code = 0u32;
        for bits in 1..16 {
            code = (code + count[bits - 1]) << 1;
            next[bits] = code;
        }
        let mut out = Self {
            codes: [0; 288],
            lens: [0; 288],
        };
        for (i, &l) in lens.iter().enumerate() {
            if l != 0 {
                let c = next[usize::from(l)];
                next[usize::from(l)] += 1;
                out.codes[i] = c.reverse_bits() >> (32 - u32::from(l));
                out.lens[i] = l;
            }
        }
        out
    }
}

/// The run-length-encoded code lengths of a dynamic block header.
struct DynamicHeader {
    hlit: usize,
    hdist: usize,
    hclen: usize,
    /// (code-length symbol, extra bits value).
    rle: Vec<(u8, u8)>,
    pre_lens: [u8; 19],
}

impl DynamicHeader {
    fn new(lit_lens: &[u8; LITLEN_SYMBOLS], dist_lens: &[u8; DIST_SYMBOLS]) -> Self {
        let hlit = 257.max(lit_lens.iter().rposition(|&l| l != 0).map_or(0, |p| p + 1));
        let hdist = 1.max(dist_lens.iter().rposition(|&l| l != 0).map_or(0, |p| p + 1));
        let mut all = Vec::with_capacity(hlit + hdist);
        all.extend_from_slice(&lit_lens[..hlit]);
        all.extend_from_slice(&dist_lens[..hdist]);

        let mut rle = Vec::with_capacity(all.len());
        let mut i = 0;
        while i < all.len() {
            let l = all[i];
            let mut run = all[i..].iter().take_while(|&&x| x == l).count();
            i += run;
            if l == 0 {
                while run >= 11 {
                    let r = run.min(138);
                    rle.push((18, (r - 11) as u8));
                    run -= r;
                }
                if run >= 3 {
                    rle.push((17, (run - 3) as u8));
                    run = 0;
                }
            } else {
                rle.push((l, 0));
                run -= 1;
                while run >= 3 {
                    let r = run.min(6);
                    rle.push((16, (r - 3) as u8));
                    run -= r;
                }
            }
            for _ in 0..run {
                rle.push((l, 0));
            }
        }

        let mut pre_freq = [0u32; 19];
        for &(sym, _) in &rle {
            pre_freq[usize::from(sym)] += 1;
        }
        let mut pre_lens = [0u8; 19];
        huffman_lengths(&pre_freq, 7, &mut pre_lens);
        let hclen = 4.max(
            PRECODE_ORDER
                .iter()
                .rposition(|&s| pre_lens[s] != 0)
                .map_or(0, |p| p + 1),
        );
        Self {
            hlit,
            hdist,
            hclen,
            rle,
            pre_lens,
        }
    }

    /// Size in bits, excluding the 3-bit block header.
    fn cost(&self) -> u64 {
        let body: u64 = self
            .rle
            .iter()
            .map(|&(sym, _)| {
                u64::from(self.pre_lens[usize::from(sym)])
                    + match sym {
                        16 => 2,
                        17 => 3,
                        18 => 7,
                        _ => 0,
                    }
            })
            .sum();
        14 + 3 * self.hclen as u64 + body
    }

    fn write(&self, w: &mut BitWriter) {
        w.put((self.hlit - 257) as u32, 5);
        w.put((self.hdist - 1) as u32, 5);
        w.put((self.hclen - 4) as u32, 4);
        for &s in &PRECODE_ORDER[..self.hclen] {
            w.put(u32::from(self.pre_lens[s]), 3);
        }
        let code = Code::new(&self.pre_lens);
        for &(sym, extra) in &self.rle {
            let s = usize::from(sym);
            w.put(code.codes[s], u32::from(code.lens[s]));
            match sym {
                16 => w.put(u32::from(extra), 2),
                17 => w.put(u32::from(extra), 3),
                18 => w.put(u32::from(extra), 7),
                _ => {}
            }
        }
    }
}

/// Computes code lengths (at most `max_bits`) for the given frequencies.
///
/// At least two symbols always get a code, as zlib ensures, so that every
/// decoder accepts the code even when only one symbol is used.
pub(super) fn huffman_lengths(freqs: &[u32], max_bits: usize, lens: &mut [u8]) {
    lens.fill(0);
    let mut syms: Vec<(u32, u16)> = freqs
        .iter()
        .enumerate()
        .filter(|&(_, &f)| f != 0)
        .map(|(i, &f)| (f, i as u16))
        .collect();
    // Pad to two symbols with the lowest unused ones.
    let mut filler = 0u16;
    while syms.len() < 2 && usize::from(filler) < freqs.len() {
        if freqs[usize::from(filler)] == 0 {
            syms.push((1, filler));
        }
        filler += 1;
    }
    if syms.len() < 2 {
        if let Some(&(_, s)) = syms.first() {
            lens[usize::from(s)] = 1;
        }
        return;
    }
    syms.sort_unstable();

    let n = syms.len();
    let mut a: Vec<u32> = syms.iter().map(|&(f, _)| f).collect();
    minimum_redundancy(&mut a);

    // Depths come out non-increasing in `a`; count them per length, folding
    // anything too long into `max_bits`, then restore the Kraft equality.
    let mut count = [0u32; 16];
    for &depth in &a {
        count[(depth as usize).min(max_bits)] += 1;
    }
    let mut total: u32 = (1..=max_bits).map(|l| count[l] << (max_bits - l)).sum();
    while total != 1 << max_bits {
        count[max_bits] -= 1;
        for l in (1..max_bits).rev() {
            if count[l] != 0 {
                count[l] -= 1;
                count[l + 1] += 2;
                break;
            }
        }
        total -= 1;
    }

    // The most frequent symbols get the shortest codes.
    let mut j = n;
    for (len, &c) in count.iter().enumerate().take(max_bits + 1).skip(1) {
        for _ in 0..c {
            j -= 1;
            lens[usize::from(syms[j].1)] = len as u8;
        }
    }
}

/// In-place minimum-redundancy code lengths (Moffat and Katajainen, 1995).
///
/// `a` holds frequencies sorted in non-decreasing order, at least two of
/// them; on return it holds each symbol's code length.
fn minimum_redundancy(a: &mut [u32]) {
    let n = a.len();
    if n < 2 {
        if let Some(x) = a.first_mut() {
            *x = 1;
        }
        return;
    }
    // Phase 1: build the tree, storing parent indices of internal nodes.
    a[0] += a[1];
    let mut root = 0;
    let mut leaf = 2;
    for next in 1..n - 1 {
        if leaf >= n || a[root] < a[leaf] {
            a[next] = a[root];
            a[root] = next as u32;
            root += 1;
        } else {
            a[next] = a[leaf];
            leaf += 1;
        }
        if leaf >= n || (root < next && a[root] < a[leaf]) {
            a[next] += a[root];
            a[root] = next as u32;
            root += 1;
        } else {
            a[next] += a[leaf];
            leaf += 1;
        }
    }
    // Phase 2: internal node depths.
    a[n - 2] = 0;
    for next in (0..n - 2).rev() {
        a[next] = a[a[next] as usize] + 1;
    }
    // Phase 3: leaf depths.
    let mut avail = 1usize;
    let mut used = 0usize;
    let mut depth = 0u32;
    let mut root = n as isize - 2;
    let mut next = n as isize - 1;
    while avail > 0 {
        while root >= 0 && a[root as usize] == depth {
            used += 1;
            root -= 1;
        }
        while avail > used {
            a[next as usize] = depth;
            next -= 1;
            avail -= 1;
        }
        avail = 2 * used;
        depth += 1;
        used = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::super::inflate::tests::{noise, texty};
    use super::super::{inflate_into, zlib_decompress_into};
    use super::*;

    fn roundtrip(data: &[u8], level: Level, chunk: usize) -> Vec<u8> {
        let z = zlib_compress_chunked(data, level, chunk);
        let mut out = vec![0u8; data.len()];
        zlib_decompress_into(&z, &mut out)
            .unwrap_or_else(|e| panic!("level {} chunk {chunk}: {e}", level.get()));
        assert!(out == data, "level {} chunk {chunk}", level.get());
        z
    }

    #[test]
    fn roundtrips_at_every_level() {
        let sets: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![7],
            b"abc".to_vec(),
            vec![0; 70_000],
            noise(40_000, 5),
            texty(150_000, 9),
            (0..100_000u32).map(|i| (i % 3) as u8).collect(),
        ];
        for data in &sets {
            for level in 0..=9 {
                for chunk in [1000, 65_536, DEFAULT_CHUNK_SIZE] {
                    roundtrip(data, Level::new(level), chunk);
                }
            }
        }
    }

    #[test]
    fn compresses_repetitive_data() {
        let data = texty(200_000, 1);
        let fast = roundtrip(&data, Level::FASTEST, DEFAULT_CHUNK_SIZE);
        let best = roundtrip(&data, Level::BEST, DEFAULT_CHUNK_SIZE);
        assert!(fast.len() < data.len() / 3, "{}", fast.len());
        assert!(best.len() <= fast.len());
        // Incompressible data costs only the stored-block overhead.
        let random = noise(100_000, 3);
        let z = roundtrip(&random, Level::DEFAULT, DEFAULT_CHUNK_SIZE);
        assert!(z.len() < random.len() + 100);
    }

    #[test]
    fn chunks_concatenate() {
        let data = texty(50_000, 4);
        let (a, b) = data.split_at(20_000);
        let mut raw = deflate_chunk(a, Level::DEFAULT, false);
        assert_eq!(&raw[raw.len() - 4..], &[0, 0, 0xff, 0xff]);
        raw.extend_from_slice(&deflate_chunk(b, Level::DEFAULT, true));
        let mut out = vec![0u8; data.len()];
        assert_eq!(inflate_into(&raw, &mut out).unwrap(), raw.len());
        assert_eq!(out, data);
    }

    #[test]
    fn output_is_independent_of_threads() {
        let data = texty(3_000_000, 11);
        let reference = zlib_compress_chunked(&data, Level::DEFAULT, 100_000);
        for threads in [1, 2, 7] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let z = pool.install(|| zlib_compress_chunked(&data, Level::DEFAULT, 100_000));
            assert!(z == reference, "{threads} threads");
        }
    }

    #[test]
    fn symbol_tables() {
        assert_eq!(length_symbol(3), (257, 0, 0));
        assert_eq!(length_symbol(10), (264, 0, 0));
        assert_eq!(length_symbol(11), (265, 1, 0));
        assert_eq!(length_symbol(12), (265, 1, 1));
        assert_eq!(length_symbol(13), (266, 1, 0));
        assert_eq!(length_symbol(19), (269, 2, 0));
        assert_eq!(length_symbol(257), (284, 5, 30));
        assert_eq!(length_symbol(258), (285, 0, 0));
        assert_eq!(distance_symbol(1), (0, 0, 0));
        assert_eq!(distance_symbol(4), (3, 0, 0));
        assert_eq!(distance_symbol(5), (4, 1, 0));
        assert_eq!(distance_symbol(7), (5, 1, 0));
        assert_eq!(distance_symbol(8), (5, 1, 1));
        assert_eq!(distance_symbol(24_577), (29, 13, 0));
        assert_eq!(distance_symbol(32_768), (29, 13, 8191));
    }

    #[test]
    fn huffman_lengths_are_valid_and_limited() {
        // Fibonacci frequencies force deep trees.
        let mut freqs = vec![0u32; 286];
        let (mut x, mut y) = (1u32, 1u32);
        for f in freqs.iter_mut().take(30) {
            *f = x;
            (x, y) = (y, x.saturating_add(y));
        }
        for max in [7, 9, 15] {
            let mut lens = vec![0u8; 286];
            huffman_lengths(&freqs, max, &mut lens);
            let kraft: f64 = lens
                .iter()
                .filter(|&&l| l != 0)
                .map(|&l| 0.5f64.powi(i32::from(l)))
                .sum();
            assert!((kraft - 1.0).abs() < 1e-9, "max {max}: kraft {kraft}");
            assert!(lens.iter().all(|&l| usize::from(l) <= max));
            assert_eq!(lens.iter().filter(|&&l| l != 0).count(), 30);
        }
        // Optimality on a small case: frequencies 1,1,2,4 → lengths 3,3,2,1.
        let mut lens = [0u8; 4];
        huffman_lengths(&[1, 1, 2, 4], 15, &mut lens);
        assert_eq!(lens, [3, 3, 2, 1]);
        // A single used symbol still gets a partner.
        let mut lens = [0u8; 30];
        huffman_lengths(&[0, 0, 5, 0], 15, &mut lens[..4]);
        assert_eq!(&lens[..4], &[1, 0, 1, 0]);
    }
}
