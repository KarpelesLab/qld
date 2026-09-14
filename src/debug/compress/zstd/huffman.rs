//! Huffman-coded literals (RFC 8878 section 4.2).

use super::bits::BackwardBits;
use super::fse;

/// Longest Huffman code Zstandard allows.
const MAX_BITS: u32 = 11;
const MAX_TABLE: usize = 1 << MAX_BITS;

/// A single-symbol decoding table indexed by the next `max_bits` bits.
pub(super) struct Table {
    max_bits: u32,
    /// (symbol, code length) per index.
    entries: [(u8, u8); MAX_TABLE],
}

impl Table {
    pub(super) const fn new() -> Self {
        Self {
            max_bits: 0,
            entries: [(0, 0); MAX_TABLE],
        }
    }

    /// Reads a Huffman tree description from the start of `data` and
    /// builds the table. Returns the bytes used.
    pub(super) fn read(&mut self, data: &[u8]) -> Result<usize, &'static str> {
        const BAD: &str = "zstd Huffman tree description";
        let (&header, rest) = data.split_first().ok_or(BAD)?;
        let mut weights = [0u8; 256];
        let (count, used) = if header < 128 {
            let size = usize::from(header);
            let body = rest.get(..size).ok_or(BAD)?;
            (read_fse_weights(body, &mut weights)?, size)
        } else {
            let count = usize::from(header).wrapping_sub(127);
            let size = count.div_ceil(2);
            let body = rest.get(..size).ok_or(BAD)?;
            for (i, w) in weights.iter_mut().take(count).enumerate() {
                let byte = body.get(i >> 1).copied().ok_or(BAD)?;
                *w = if i & 1 == 0 { byte >> 4 } else { byte & 0xf };
            }
            (count, size)
        };
        self.build(weights.get_mut(..=count).ok_or(BAD)?)?;
        Ok(used.wrapping_add(1))
    }

    /// Builds the table from `weights`, whose last element is filled in
    /// here (it is implied by the others).
    fn build(&mut self, weights: &mut [u8]) -> Result<(), &'static str> {
        const BAD: &str = "zstd Huffman weights";
        let (last, given) = weights.split_last_mut().ok_or(BAD)?;
        let mut rank = [0u32; 13];
        let mut total = 0u32;
        for &w in given.iter() {
            if u32::from(w) > MAX_BITS {
                return Err(BAD);
            }
            if w > 0 {
                total = total.wrapping_add(1 << (w & 15).saturating_sub(1));
                rank[usize::from(w)] = rank[usize::from(w)].wrapping_add(1);
            }
        }
        if total == 0 {
            return Err(BAD);
        }
        let max_bits = 32u32.wrapping_sub(total.leading_zeros());
        if max_bits > MAX_BITS {
            return Err(BAD);
        }
        let rest = (1u32 << max_bits).wrapping_sub(total);
        if !rest.is_power_of_two() {
            return Err(BAD);
        }
        let last_weight = rest.trailing_zeros().wrapping_add(1);
        *last = last_weight as u8;
        let slot = rank.get_mut(last_weight as usize).ok_or(BAD)?;
        *slot = slot.wrapping_add(1);
        if rank[1] < 2 || rank[1] & 1 != 0 {
            return Err(BAD);
        }

        let mut start = [0usize; 13];
        let mut next = 0usize;
        for w in 1..=max_bits as usize {
            start[w] = next;
            next = next.wrapping_add((rank[w] as usize) << w.saturating_sub(1));
        }
        for (symbol, &w) in weights.iter().enumerate() {
            if w == 0 {
                continue;
            }
            let w = usize::from(w);
            let len = 1usize << w.saturating_sub(1);
            let bits = (max_bits as usize).wrapping_add(1).wrapping_sub(w) as u8;
            let begin = start[w];
            let end = begin.wrapping_add(len);
            for entry in self.entries.get_mut(begin..end).ok_or(BAD)? {
                *entry = (symbol as u8, bits);
            }
            start[w] = end;
        }
        self.max_bits = max_bits;
        Ok(())
    }

    /// Decodes `out.len()` literals from one backward bitstream, which must
    /// be consumed exactly.
    fn decode_stream(&self, stream: &[u8], out: &mut [u8]) -> Result<(), &'static str> {
        let mut bits = BackwardBits::new(stream)?;
        let max_bits = self.max_bits;
        let shift = 64u32.wrapping_sub(max_bits) & 63;
        // Fast path: one refill leaves 57 bits, enough for four codes of at
        // most 11 bits.
        let (quads, _) = out.as_chunks_mut::<4>();
        let mut done = 0usize;
        for quad in quads {
            let Some((container, mut consumed)) = bits.refill_fast() else {
                break;
            };
            for slot in quad {
                let index = ((container << consumed) >> shift) as usize & (MAX_TABLE - 1);
                let (symbol, len) = self.entries[index];
                consumed = consumed.wrapping_add(u32::from(len));
                *slot = symbol;
            }
            bits.set_consumed(consumed);
            done = done.wrapping_add(4);
        }
        for slot in out.get_mut(done..).unwrap_or_default() {
            let (symbol, len) = self.entries[(bits.peek(max_bits) as usize) & (MAX_TABLE - 1)];
            bits.consume(u32::from(len));
            *slot = symbol;
        }
        if !bits.finished() {
            return Err("zstd Huffman literal stream (bad length)");
        }
        Ok(())
    }

    /// Decodes literals from one stream or from four (with a jump table)
    /// into `out`, which has the regenerated size.
    pub(super) fn decode(
        &self,
        data: &[u8],
        four: bool,
        out: &mut [u8],
    ) -> Result<(), &'static str> {
        const BAD: &str = "zstd Huffman literal streams";
        if self.max_bits == 0 {
            return Err("zstd treeless literals (no previous Huffman table)");
        }
        if !four {
            return self.decode_stream(data, out);
        }
        let (jump, streams) = data.split_at_checked(6).ok_or(BAD)?;
        let size = |i: usize| usize::from(u16::from_le_bytes([jump[i], jump[i | 1]]));
        let (s1, s2, s3) = (size(0), size(2), size(4));
        let (a, rest) = streams.split_at_checked(s1).ok_or(BAD)?;
        let (b, rest) = rest.split_at_checked(s2).ok_or(BAD)?;
        let (c, d) = rest.split_at_checked(s3).ok_or(BAD)?;
        if out.len() < 6 {
            return Err(BAD);
        }
        let segment = out.len().div_ceil(4);
        let (oa, rest) = out.split_at_mut(segment);
        let (ob, rest) = rest.split_at_mut(segment);
        let (oc, od) = rest.split_at_mut(segment);
        self.decode_stream(a, oa)?;
        self.decode_stream(b, ob)?;
        self.decode_stream(c, oc)?;
        self.decode_stream(d, od)
    }
}

/// Decodes FSE-compressed Huffman weights. Returns the number of weights.
fn read_fse_weights(data: &[u8], weights: &mut [u8; 256]) -> Result<usize, &'static str> {
    const BAD: &str = "zstd Huffman weights";
    let mut norm = [0i16; 256];
    let (used, log, symbols) = fse::read_ncount(data, 255, 6, &mut norm)?;
    let mut table = fse::Table::new();
    table.build(norm.get(..symbols).ok_or(BAD)?, log)?;
    let mut bits = BackwardBits::new(data.get(used..).ok_or(BAD)?)?;

    // Two interleaved states share the table and the bitstream. When an
    // update runs past the start of the stream, the other state's symbol
    // is the last one.
    let mut state1 = bits.read(log);
    let mut state2 = bits.read(log);
    let mut count = 0usize;
    let mut push = |symbol: u8, count: &mut usize| -> Result<(), &'static str> {
        *weights.get_mut(*count).ok_or(BAD)? = symbol;
        *count = count.wrapping_add(1);
        if *count > 255 { Err(BAD) } else { Ok(()) }
    };
    loop {
        let e = table.get(state1);
        push(e.symbol, &mut count)?;
        state1 = u64::from(e.base).wrapping_add(bits.read(u32::from(e.bits)));
        if bits.overflowed() {
            push(table.get(state2).symbol, &mut count)?;
            break;
        }
        let e = table.get(state2);
        push(e.symbol, &mut count)?;
        state2 = u64::from(e.base).wrapping_add(bits.read(u32::from(e.bits)));
        if bits.overflowed() {
            push(table.get(state1).symbol, &mut count)?;
            break;
        }
    }
    Ok(count)
}
