//! DEFLATE (RFC 1951) and zlib (RFC 1950) decompression.
//!
//! The output size is always known in advance (from an ELF compression
//! header or a `.zdebug` header), so the decoder writes into a caller-sized
//! `&mut [u8]` and treats any size mismatch as corruption.
//!
//! # Decoding model
//!
//! - A 64-bit bit buffer is refilled eight bytes at a time with one
//!   little-endian load (the "branchless refill" of libdeflate), leaving at
//!   least 56 valid bits. That is enough for a whole length/distance pair
//!   (at most 15 + 5 + 15 + 13 = 48 bits), so the hot loop refills once per
//!   symbol. Near the end of the input the refill falls back to one byte at
//!   a time and pads with virtual zero bytes, which are counted and turned
//!   into a truncation error if the stream actually consumes them.
//! - Huffman codes decode through a table indexed by the next
//!   [`LITLEN_BITS`] (or [`DIST_BITS`]) bits. Each 32-bit entry holds the
//!   decoded value (literal, length base or distance base), the extra-bit
//!   count and the code length, so decoding a symbol is one load. Longer
//!   codes go through a second-level table, as in zlib's `inflate_table`.
//! - Matches whose source is at least eight bytes back are copied eight
//!   bytes at a time, overshooting into output that later symbols overwrite.
//!   Short-distance matches (runs) use `fill` or doubling `copy_within`.
//!
//! Primary-table lookups index fixed-size arrays with a masked value, so the
//! compiler removes their bounds checks; everything else is checked and
//! reports a [`DecodeError`] rather than panicking.

use std::sync::OnceLock;

use super::DecodeError;
use super::adler32::adler32;
use super::copy::copy_match;

/// Bits indexed by the primary literal/length table.
const LITLEN_BITS: u32 = 11;
/// Bits indexed by the primary distance table.
const DIST_BITS: u32 = 8;
/// Bits indexed by the code-length (precode) table; its codes are at most
/// 7 bits, so it never needs a second level.
const PRECODE_BITS: u32 = 7;

/// Table sizes large enough for any valid code with the primary sizes above
/// (zlib's `enough` program: 2342 for 288 symbols / 11 bits / 15 max, 402 for
/// 32 symbols / 8 bits / 15 max). Building a table checks the bound anyway.
const LITLEN_ENOUGH: usize = 2342;
const DIST_ENOUGH: usize = 402;
const PRECODE_ENOUGH: usize = 128;

const LITLEN_MASK: u64 = 0x7ff;
const DIST_MASK: u64 = 0xff;
const PRECODE_MASK: u64 = 0x7f;

// Table entry layout: value << 16 | extra << 8 | flags | code bits.
const F_LITERAL: u32 = 0x10;
const F_EOB: u32 = 0x20;
const F_SUBTABLE: u32 = 0x40;
const F_INVALID: u32 = 0x80;
const INVALID: u32 = F_INVALID;

#[inline(always)]
const fn entry(value: u32, extra: u32, flags: u32, bits: u32) -> u32 {
    (value << 16) | (extra << 8) | flags | bits
}

#[inline(always)]
const fn entry_bits(e: u32) -> u32 {
    e & 0xf
}

#[inline(always)]
const fn entry_extra(e: u32) -> u32 {
    (e >> 8) & 0xff
}

#[inline(always)]
const fn entry_value(e: u32) -> u32 {
    e >> 16
}

/// `n` low bits set.
#[inline(always)]
const fn low_mask(n: u32) -> u64 {
    !u64::MAX.wrapping_shl(n)
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Order in which code-length code lengths are transmitted.
const PRECODE_ORDER: [u8; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableKind {
    Precode,
    Litlen,
    Dist,
}

impl TableKind {
    #[inline]
    fn entry(self, symbol: u16, bits: u32) -> u32 {
        let s = usize::from(symbol);
        match self {
            Self::Precode => entry(u32::from(symbol), 0, 0, bits),
            Self::Litlen => match s {
                0..=255 => entry(u32::from(symbol), 0, F_LITERAL, bits),
                256 => entry(0, 0, F_EOB, bits),
                _ => match (
                    LENGTH_BASE.get(s.wrapping_sub(257)),
                    LENGTH_EXTRA.get(s.wrapping_sub(257)),
                ) {
                    (Some(&base), Some(&extra)) => {
                        entry(u32::from(base), u32::from(extra), 0, bits)
                    }
                    _ => entry(0, 0, F_INVALID, bits),
                },
            },
            Self::Dist => match (DIST_BASE.get(s), DIST_EXTRA.get(s)) {
                (Some(&base), Some(&extra)) => entry(u32::from(base), u32::from(extra), 0, bits),
                _ => entry(0, 0, F_INVALID, bits),
            },
        }
    }
}

/// Builds a decoding table for the code with the given code lengths
/// (canonical Huffman, RFC 1951 section 3.2.2), in the layout described in
/// the module documentation. This follows zlib's `inflate_table`.
///
/// Over-subscribed codes are rejected. Incomplete codes are rejected unless
/// they consist of a single one-bit code (allowed by zlib for literal/length
/// and distance codes); unused entries decode as invalid.
fn build_table(
    lens: &[u8],
    table: &mut [u32],
    root: u32,
    kind: TableKind,
) -> Result<(), &'static str> {
    table.fill(INVALID);
    let mut count = [0u16; 16];
    for &len in lens {
        let slot = count
            .get_mut(usize::from(len))
            .ok_or("Huffman code length")?;
        *slot = slot.wrapping_add(1);
    }
    count[0] = 0;
    let Some(max) = (1..16u32).rev().find(|&l| count[l as usize] != 0) else {
        // No codes at all: every entry is invalid, which is only an error if
        // a symbol is actually decoded.
        return Ok(());
    };
    let min = (1..16u32).find(|&l| count[l as usize] != 0).unwrap_or(max);

    let mut left: i32 = 1;
    for &c in &count[1..] {
        left = left.wrapping_shl(1).wrapping_sub(i32::from(c));
        if left < 0 {
            return Err("Huffman code (over-subscribed)");
        }
    }
    if left > 0 && (kind == TableKind::Precode || max != 1) {
        return Err("Huffman code (incomplete)");
    }

    // Symbols sorted by code length, then by value.
    let mut offs = [0u16; 16];
    for len in 1..15usize {
        offs[len.wrapping_add(1)] = offs[len].wrapping_add(count[len]);
    }
    let mut work = [0u16; 320];
    for (symbol, &len) in lens.iter().enumerate() {
        if len != 0 {
            let at = &mut offs[usize::from(len)];
            if let Some(slot) = work.get_mut(usize::from(*at)) {
                *slot = u16::try_from(symbol).unwrap_or(u16::MAX);
            }
            *at = at.wrapping_add(1);
        }
    }

    let mut huff: u32 = 0; // bit-reversed code being assigned
    let mut len = min;
    let mut sym = 0usize;
    let mut next = 0usize; // start of the current (sub)table
    let mut curr = root; // index bits of the current table
    let mut drop = 0u32; // bits already consumed before the current table
    let mut used = 1usize << root;
    let mask = (1u32 << root).wrapping_sub(1);
    let mut low = u32::MAX;

    loop {
        let symbol = *work.get(sym).ok_or("Huffman table")?;
        let here = kind.entry(symbol, len.wrapping_sub(drop));
        // Replicate for every index whose low (len - drop) bits are `huff`.
        let incr = 1usize.wrapping_shl(len.wrapping_sub(drop));
        let size = 1usize.wrapping_shl(curr);
        let base = next.wrapping_add((huff >> drop) as usize);
        let mut fill = size;
        while fill != 0 {
            fill = fill.wrapping_sub(incr);
            let slot = table
                .get_mut(base.wrapping_add(fill))
                .ok_or("Huffman table (too large)")?;
            *slot = here;
        }

        // Increment the reversed `len`-bit code.
        let mut step = 1u32.wrapping_shl(len.wrapping_sub(1));
        while huff & step != 0 {
            step >>= 1;
        }
        if step != 0 {
            huff &= step.wrapping_sub(1);
            huff = huff.wrapping_add(step);
        } else {
            huff = 0;
        }

        sym = sym.wrapping_add(1);
        let c = &mut count[len as usize & 15];
        *c = c.wrapping_sub(1);
        if *c == 0 {
            if len == max {
                break;
            }
            let symbol = *work.get(sym).ok_or("Huffman table")?;
            len = u32::from(*lens.get(usize::from(symbol)).ok_or("Huffman table")?);
        }

        // Start a new second-level table when the root bits change.
        if len > root && (huff & mask) != low {
            if drop == 0 {
                drop = root;
            }
            next = next.wrapping_add(size);
            curr = len.wrapping_sub(drop);
            let mut left = 1i32.wrapping_shl(curr);
            while curr.wrapping_add(drop) < max {
                left = left.wrapping_sub(i32::from(count[curr.wrapping_add(drop) as usize & 15]));
                if left <= 0 {
                    break;
                }
                curr = curr.wrapping_add(1);
                left = left.wrapping_shl(1);
            }
            used = used.wrapping_add(1usize.wrapping_shl(curr));
            if used > table.len() {
                return Err("Huffman table (too large)");
            }
            low = huff & mask;
            let slot = table
                .get_mut(low as usize)
                .ok_or("Huffman table (too large)")?;
            *slot = entry(
                u32::try_from(next).map_err(|_| "Huffman table (too large)")?,
                curr,
                F_SUBTABLE,
                root,
            );
        }
    }
    Ok(())
}

/// The decoding tables for one Huffman-coded block.
struct Tables {
    litlen: [u32; LITLEN_ENOUGH],
    dist: [u32; DIST_ENOUGH],
}

impl Tables {
    const fn new() -> Self {
        Self {
            litlen: [INVALID; LITLEN_ENOUGH],
            dist: [INVALID; DIST_ENOUGH],
        }
    }
}

/// The tables for fixed-Huffman blocks (RFC 1951 section 3.2.6), built once.
fn fixed_tables() -> &'static Tables {
    static FIXED: OnceLock<Box<Tables>> = OnceLock::new();
    FIXED.get_or_init(|| {
        let mut lens = [0u8; 320];
        lens[..144].fill(8);
        lens[144..256].fill(9);
        lens[256..280].fill(7);
        lens[280..288].fill(8);
        lens[288..320].fill(5);
        let mut tables = Box::new(Tables::new());
        // Both codes are complete, so building cannot fail.
        let _ = build_table(
            &lens[..288],
            &mut tables.litlen,
            LITLEN_BITS,
            TableKind::Litlen,
        );
        let _ = build_table(&lens[288..], &mut tables.dist, DIST_BITS, TableKind::Dist);
        tables
    })
}

/// LSB-first bit reader over the compressed input.
struct Bits<'a> {
    data: &'a [u8],
    /// Next byte to load into `buf`.
    pos: usize,
    buf: u64,
    /// Valid bits in `buf`. Bits above them are zero or the low bits of
    /// `data[pos]`, so reloading those bits is harmless.
    left: u32,
    /// Virtual zero bytes loaded past the end of `data`.
    overrun: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            buf: 0,
            left: 0,
            overrun: 0,
        }
    }

    /// Ensures at least 56 valid bits.
    #[inline(always)]
    fn refill(&mut self) -> Result<(), DecodeError> {
        if let Some(word) = self.data.get(self.pos..self.pos.wrapping_add(8))
            && let Ok(word) = <[u8; 8]>::try_from(word)
        {
            self.buf |= u64::from_le_bytes(word).wrapping_shl(self.left);
            self.pos = self
                .pos
                .wrapping_add((63u32.wrapping_sub(self.left) >> 3) as usize);
            self.left |= 56;
            Ok(())
        } else {
            self.refill_slow()
        }
    }

    #[cold]
    #[inline(never)]
    fn refill_slow(&mut self) -> Result<(), DecodeError> {
        while self.left <= 55 {
            if let Some(&byte) = self.data.get(self.pos) {
                self.buf |= u64::from(byte).wrapping_shl(self.left);
                self.pos = self.pos.wrapping_add(1);
            } else {
                self.overrun = self.overrun.wrapping_add(1);
                if self.overrun > 8 {
                    return Err(DecodeError::new(
                        self.data.len(),
                        "deflate stream (truncated)",
                    ));
                }
            }
            self.left = self.left.wrapping_add(8);
        }
        Ok(())
    }

    #[inline(always)]
    fn consume(&mut self, n: u32) {
        self.buf = self.buf.wrapping_shr(n);
        self.left = self.left.wrapping_sub(n);
    }

    /// Reads `n` (at most 32) bits; the caller has refilled.
    #[inline(always)]
    fn take(&mut self, n: u32) -> u32 {
        let value = (self.buf & low_mask(n)) as u32;
        self.consume(n);
        value
    }

    /// An approximate input offset, for error messages.
    fn offset(&self) -> usize {
        self.pos
            .saturating_sub((self.left / 8) as usize)
            .min(self.data.len())
    }

    /// Discards bits up to a byte boundary and hands the whole bytes still
    /// buffered back to the input. Returns the input position.
    fn align(&mut self) -> Result<usize, DecodeError> {
        self.consume(self.left % 8);
        let buffered = self.left / 8;
        let Some(real) = buffered.checked_sub(self.overrun) else {
            return Err(DecodeError::new(
                self.data.len(),
                "deflate stream (truncated)",
            ));
        };
        self.pos = self.pos.saturating_sub(real as usize);
        self.buf = 0;
        self.left = 0;
        self.overrun = 0;
        Ok(self.pos)
    }
}

/// Decompresses a raw DEFLATE stream into `out`, which must be filled
/// exactly. Returns the number of input bytes the stream occupied.
///
/// # Errors
///
/// Returns a [`DecodeError`] if the stream is corrupt or truncated, or does
/// not decompress to exactly `out.len()` bytes.
pub fn inflate_into(input: &[u8], out: &mut [u8]) -> Result<usize, DecodeError> {
    let (consumed, written) = inflate_stream(input, out)?;
    if written != out.len() {
        return Err(DecodeError::new(
            consumed,
            "compressed data (smaller than the declared size)",
        ));
    }
    Ok(consumed)
}

/// Decodes one DEFLATE stream into the start of `out`. Returns the input
/// bytes consumed and the output bytes written.
fn inflate_stream(input: &[u8], out: &mut [u8]) -> Result<(usize, usize), DecodeError> {
    let mut bits = Bits::new(input);
    let mut o = 0usize;
    let mut dynamic: Option<Box<Tables>> = None;
    loop {
        bits.refill()?;
        let last = bits.take(1) == 1;
        match bits.take(2) {
            0 => o = stored_block(&mut bits, out, o)?,
            1 => {
                let t = fixed_tables();
                o = huffman_block(&mut bits, &t.litlen, &t.dist, out, o)?;
            }
            2 => {
                let t = dynamic.get_or_insert_with(|| Box::new(Tables::new()));
                read_dynamic_header(&mut bits, t)?;
                o = huffman_block(&mut bits, &t.litlen, &t.dist, out, o)?;
            }
            _ => return Err(DecodeError::new(bits.offset(), "deflate block type")),
        }
        if last {
            let end = bits.align()?;
            return Ok((end, o));
        }
    }
}

#[cold]
fn too_large(at: usize) -> DecodeError {
    DecodeError::new(at, "compressed data (larger than the declared size)")
}

fn stored_block(bits: &mut Bits<'_>, out: &mut [u8], o: usize) -> Result<usize, DecodeError> {
    let at = bits.align()?;
    let header = at
        .checked_add(4)
        .and_then(|end| bits.data.get(at..end))
        .ok_or_else(|| DecodeError::new(at, "stored block header (truncated)"))?;
    let len = u16::from_le_bytes([header[0], header[1]]);
    let nlen = u16::from_le_bytes([header[2], header[3]]);
    if len != !nlen {
        return Err(DecodeError::new(at, "stored block length"));
    }
    let start = at.wrapping_add(4);
    let len = usize::from(len);
    let src = start
        .checked_add(len)
        .and_then(|end| bits.data.get(start..end))
        .ok_or_else(|| DecodeError::new(start, "stored block (truncated)"))?;
    let end = o.checked_add(len).ok_or_else(|| too_large(start))?;
    out.get_mut(o..end)
        .ok_or_else(|| too_large(start))?
        .copy_from_slice(src);
    bits.pos = start.wrapping_add(len);
    Ok(end)
}

fn read_dynamic_header(bits: &mut Bits<'_>, t: &mut Tables) -> Result<(), DecodeError> {
    bits.refill()?;
    let at = bits.offset();
    let hlit = bits.take(5) as usize + 257;
    let hdist = bits.take(5) as usize + 1;
    let hclen = bits.take(4) as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(DecodeError::new(
            at,
            "dynamic block header (too many codes)",
        ));
    }

    let mut pre_lens = [0u8; 19];
    for &index in PRECODE_ORDER.iter().take(hclen) {
        bits.refill()?;
        pre_lens[usize::from(index)] = bits.take(3) as u8;
    }
    let mut pre = [INVALID; PRECODE_ENOUGH];
    build_table(&pre_lens, &mut pre, PRECODE_BITS, TableKind::Precode)
        .map_err(|what| DecodeError::new(at, what))?;

    let total = hlit.wrapping_add(hdist);
    let mut lens = [0u8; 320];
    let mut i = 0usize;
    while i < total {
        bits.refill()?;
        let e = pre[(bits.buf & PRECODE_MASK) as usize];
        if e & F_INVALID != 0 {
            return Err(DecodeError::new(bits.offset(), "code length code"));
        }
        bits.consume(entry_bits(e));
        let symbol = entry_value(e);
        let (value, repeat) = match symbol {
            0..=15 => (symbol as u8, 1),
            16 => {
                let Some(&prev) = i.checked_sub(1).and_then(|p| lens.get(p)) else {
                    return Err(DecodeError::new(bits.offset(), "code length repeat"));
                };
                (prev, 3 + bits.take(2) as usize)
            }
            17 => (0, 3 + bits.take(3) as usize),
            _ => (0, 11 + bits.take(7) as usize),
        };
        let end = i.wrapping_add(repeat);
        if end > total {
            return Err(DecodeError::new(bits.offset(), "code length repeat"));
        }
        lens[i..end].fill(value);
        i = end;
    }
    if lens[256] == 0 {
        return Err(DecodeError::new(at, "dynamic block (no end-of-block code)"));
    }
    build_table(&lens[..hlit], &mut t.litlen, LITLEN_BITS, TableKind::Litlen)
        .map_err(|what| DecodeError::new(at, what))?;
    build_table(&lens[hlit..total], &mut t.dist, DIST_BITS, TableKind::Dist)
        .map_err(|what| DecodeError::new(at, what))?;
    Ok(())
}

/// Decodes the symbols of one Huffman-coded block. Returns the new output
/// position.
#[inline]
fn huffman_block(
    bits: &mut Bits<'_>,
    litlen: &[u32; LITLEN_ENOUGH],
    dist: &[u32; DIST_ENOUGH],
    out: &mut [u8],
    mut o: usize,
) -> Result<usize, DecodeError> {
    loop {
        bits.refill()?;
        let mut e = litlen[(bits.buf & LITLEN_MASK) as usize];
        if e & F_SUBTABLE != 0 {
            bits.consume(LITLEN_BITS);
            let index = (entry_value(e) as usize)
                .wrapping_add((bits.buf & low_mask(entry_extra(e))) as usize);
            e = litlen.get(index).copied().unwrap_or(INVALID);
        }
        if e & F_LITERAL != 0 {
            bits.consume(entry_bits(e));
            let Some(slot) = out.get_mut(o) else {
                return Err(too_large(bits.offset()));
            };
            *slot = entry_value(e) as u8;
            o = o.wrapping_add(1);
            // At least 41 bits remain: room for a second primary-table literal.
            let e = litlen[(bits.buf & LITLEN_MASK) as usize];
            if e & F_LITERAL != 0 {
                bits.consume(entry_bits(e));
                let Some(slot) = out.get_mut(o) else {
                    return Err(too_large(bits.offset()));
                };
                *slot = entry_value(e) as u8;
                o = o.wrapping_add(1);
            }
            continue;
        }
        if e & (F_EOB | F_INVALID) != 0 {
            if e & F_EOB != 0 {
                bits.consume(entry_bits(e));
                return Ok(o);
            }
            return Err(DecodeError::new(bits.offset(), "literal/length code"));
        }

        let code_bits = entry_bits(e);
        let length = (entry_value(e) as usize)
            .wrapping_add((bits.buf.wrapping_shr(code_bits) & low_mask(entry_extra(e))) as usize);
        bits.consume(code_bits.wrapping_add(entry_extra(e)));

        let mut d = dist[(bits.buf & DIST_MASK) as usize];
        if d & F_SUBTABLE != 0 {
            bits.consume(DIST_BITS);
            let index = (entry_value(d) as usize)
                .wrapping_add((bits.buf & low_mask(entry_extra(d))) as usize);
            d = dist.get(index).copied().unwrap_or(INVALID);
        }
        if d & F_INVALID != 0 {
            return Err(DecodeError::new(bits.offset(), "distance code"));
        }
        let code_bits = entry_bits(d);
        let distance = (entry_value(d) as usize)
            .wrapping_add((bits.buf.wrapping_shr(code_bits) & low_mask(entry_extra(d))) as usize);
        bits.consume(code_bits.wrapping_add(entry_extra(d)));

        o = copy_match(out, o, distance, length).ok_or_else(|| {
            if distance > o {
                DecodeError::new(bits.offset(), "match distance (too far back)")
            } else {
                too_large(bits.offset())
            }
        })?;
    }
}

/// Decompresses zlib data (RFC 1950) into `out`, which must be filled
/// exactly, and verifies the Adler-32 checksum.
///
/// Several zlib streams in a row are decoded one after another until `out`
/// is full, as GNU binutils does. Bytes after the stream that fills `out`
/// are ignored, as zlib's `uncompress` does.
///
/// # Errors
///
/// Returns a [`DecodeError`] for a bad header, a preset dictionary, corrupt
/// or truncated data, a checksum mismatch, or a size other than
/// `out.len()`.
pub fn zlib_decompress_into(input: &[u8], out: &mut [u8]) -> Result<(), DecodeError> {
    let mut at = 0usize;
    let mut o = 0usize;
    loop {
        let Some(&[cmf, flg]) = input.get(at..at.wrapping_add(2)) else {
            return Err(DecodeError::new(at, "zlib header (truncated)"));
        };
        if cmf & 0x0f != 8 || cmf >> 4 > 7 || (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
            return Err(DecodeError::new(at, "zlib header"));
        }
        if flg & 0x20 != 0 {
            return Err(DecodeError::new(at, "zlib header (preset dictionary)"));
        }
        let body = at.wrapping_add(2);
        let rest = input.get(body..).unwrap_or_default();
        let window = out.get_mut(o..).unwrap_or_default();
        let (consumed, written) = inflate_stream(rest, window).map_err(|e| e.shifted(body))?;
        let trailer = body.wrapping_add(consumed);
        let Some(&[c0, c1, c2, c3]) = input.get(trailer..trailer.wrapping_add(4)) else {
            return Err(DecodeError::new(trailer, "zlib checksum (truncated)"));
        };
        let expected = u32::from_be_bytes([c0, c1, c2, c3]);
        let end = o.wrapping_add(written);
        if adler32(out.get(o..end).unwrap_or_default()) != expected {
            return Err(DecodeError::new(trailer, "zlib data (Adler-32 mismatch)"));
        }
        o = end;
        at = trailer.wrapping_add(4);
        if o == out.len() {
            return Ok(());
        }
        if at >= input.len() {
            return Err(DecodeError::new(
                at,
                "compressed data (smaller than the declared size)",
            ));
        }
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
pub(super) mod tests {
    use super::*;

    /// Deterministic xorshift noise.
    pub(crate) fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        seed |= 1;
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 24) as u8
            })
            .collect()
    }

    /// Text-like data with plenty of repeats at all distances.
    pub(crate) fn texty(len: usize, seed: u64) -> Vec<u8> {
        const WORDS: &[&[u8]] = &[
            b"DW_TAG_subprogram ",
            b"DW_AT_name ",
            b".debug_info",
            b"\0\0\0\0",
            b"static inline int ",
            b"return 0;\n",
            b"/usr/include/stdio.h",
            b"\x01\x02\x03",
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            b"std::vector<std::string>::iterator ",
        ];
        let picks = noise(len, seed);
        let mut out = Vec::with_capacity(len + 64);
        let mut i = 0;
        while out.len() < len {
            let p = usize::from(picks[i % picks.len()]);
            i += 1;
            if p < 20 {
                // A random byte now and then.
                out.push(picks[(i * 7) % picks.len()]);
            } else {
                out.extend_from_slice(WORDS[p % WORDS.len()]);
            }
        }
        out.truncate(len);
        out
    }

    /// Writes a stored-only zlib stream (to test the decoder without the
    /// encoder).
    fn stored_zlib(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut chunks = data.chunks(65_535).peekable();
        if chunks.peek().is_none() {
            out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
        }
        while let Some(chunk) = chunks.next() {
            out.push(u8::from(chunks.peek().is_none()));
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(&adler32(data).to_be_bytes());
        out
    }

    fn decode(input: &[u8], size: usize) -> Result<Vec<u8>, DecodeError> {
        let mut out = vec![0; size];
        zlib_decompress_into(input, &mut out).map(|()| out)
    }

    #[test]
    fn stored_blocks() {
        for len in [0, 1, 100, 65_535, 65_536, 200_000] {
            let data = noise(len, len as u64);
            assert_eq!(decode(&stored_zlib(&data), len).unwrap(), data);
        }
    }

    /// `zlib.compress(b"hello hello hello hello", 9)`: a fixed-Huffman block
    /// with an overlapping match.
    #[test]
    fn fixed_block_vector() {
        let input = [
            0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01, 0x68, 0x03,
            0x08, 0xb1,
        ];
        assert_eq!(
            decode(&input, 23).unwrap(),
            b"hello hello hello hello".to_vec()
        );
    }

    #[test]
    fn empty_stream() {
        // zlib.compress(b"")
        let input = [0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01];
        assert_eq!(decode(&input, 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn size_mismatch_is_an_error() {
        let data = texty(1000, 3);
        let z = stored_zlib(&data);
        assert!(decode(&z, 999).is_err());
        assert!(decode(&z, 1001).is_err());
        let fixed = [
            0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01, 0x68, 0x03,
            0x08, 0xb1,
        ];
        assert!(decode(&fixed, 22).is_err());
        assert!(decode(&fixed, 24).is_err());
    }

    #[test]
    fn checksum_mismatch_is_an_error() {
        let data = texty(1000, 3);
        let mut z = stored_zlib(&data);
        let n = z.len();
        z[n - 1] ^= 1;
        let err = decode(&z, 1000).unwrap_err();
        assert!(err.what.contains("Adler-32"), "{err:?}");
    }

    #[test]
    fn concatenated_streams() {
        let (a, b) = (texty(5000, 1), noise(3000, 2));
        let mut z = stored_zlib(&a);
        z.extend_from_slice(&stored_zlib(&b));
        let mut both = a.clone();
        both.extend_from_slice(&b);
        assert_eq!(decode(&z, 8000).unwrap(), both);
    }

    #[test]
    fn rejects_bad_codes() {
        // Over-subscribed: three codes of length 1.
        let mut table = [0u32; LITLEN_ENOUGH];
        assert!(build_table(&[1, 1, 1], &mut table, LITLEN_BITS, TableKind::Litlen).is_err());
        // Incomplete precode.
        let mut table = [0u32; PRECODE_ENOUGH];
        assert!(build_table(&[2, 2, 2], &mut table, PRECODE_BITS, TableKind::Precode).is_err());
        // A single one-bit distance code is allowed.
        let mut table = [0u32; DIST_ENOUGH];
        assert!(build_table(&[1], &mut table, DIST_BITS, TableKind::Dist).is_ok());
    }

    #[test]
    fn long_codes_use_subtables() {
        // A complete code with lengths up to 15 (a Fibonacci-like skew).
        let mut lens = vec![0u8; 288];
        for (i, l) in lens.iter_mut().enumerate().take(15) {
            *l = (i + 1) as u8;
        }
        lens[15] = 15;
        let mut table = [0u32; LITLEN_ENOUGH];
        build_table(&lens, &mut table, LITLEN_BITS, TableKind::Litlen).unwrap();
        // Symbol 14 has the code 111111111111110 (15 bits); reversed, its low
        // 11 bits select a subtable.
        let code: u32 = 0b111_1111_1111_1110;
        let reversed = code.reverse_bits() >> 17;
        let e = table[(reversed & 0x7ff) as usize];
        assert_ne!(e & F_SUBTABLE, 0);
        let sub = table
            [entry_value(e) as usize + ((reversed >> 11) & ((1 << entry_extra(e)) - 1)) as usize];
        assert_eq!(entry_value(sub), 14);
        assert_eq!(entry_bits(sub), 4);
    }
}
