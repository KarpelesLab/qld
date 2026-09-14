//! Bit readers for Zstandard: forward (FSE table descriptions) and backward
//! (Huffman and FSE bitstreams, RFC 8878 section 4.1).

/// Reads a backward bitstream: the stream is read from its last byte
/// towards its first, and the highest set bit of the last byte marks the
/// start.
///
/// A 64-bit container holds the eight bytes ending at `ptr + 8`; `consumed`
/// counts the bits already taken from its top. Reads past the beginning of
/// the stream return zero bits and push `consumed` above 64, which
/// [`overflowed`](Self::overflowed) reports.
pub(super) struct BackwardBits<'a> {
    data: &'a [u8],
    container: u64,
    consumed: u32,
    ptr: usize,
}

#[inline(always)]
fn load(data: &[u8], at: usize) -> u64 {
    match data.get(at..at.wrapping_add(8)) {
        Some(bytes) => u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])),
        None => 0,
    }
}

impl<'a> BackwardBits<'a> {
    /// Starts reading `data`. Fails if it is empty or its last byte is 0
    /// (no start marker).
    pub(super) fn new(data: &'a [u8]) -> Result<Self, &'static str> {
        let Some(&last) = data.last() else {
            return Err("zstd bitstream (empty)");
        };
        if last == 0 {
            return Err("zstd bitstream (missing end marker)");
        }
        // Skip the marker bit and the zero bits above it.
        let marker = last.leading_zeros().wrapping_add(1);
        if let Some(ptr) = data.len().checked_sub(8) {
            Ok(Self {
                data,
                container: load(data, ptr),
                consumed: marker,
                ptr,
            })
        } else {
            let mut bytes = [0u8; 8];
            if let Some(dst) = bytes.get_mut(..data.len()) {
                dst.copy_from_slice(data);
            }
            // The missing high bytes count as already consumed.
            let missing = u32::try_from(8usize.wrapping_sub(data.len())).unwrap_or(8);
            Ok(Self {
                data,
                container: u64::from_le_bytes(bytes),
                consumed: marker.wrapping_add(missing.wrapping_mul(8)),
                ptr: 0,
            })
        }
    }

    /// Moves the container back over consumed whole bytes.
    #[inline(always)]
    fn reload(&mut self) {
        if self.consumed > 64 {
            return;
        }
        if self.ptr >= 8 {
            self.ptr = self.ptr.wrapping_sub((self.consumed >> 3) as usize);
            self.consumed &= 7;
        } else if self.ptr == 0 {
            return;
        } else {
            let bytes = ((self.consumed >> 3) as usize).min(self.ptr);
            self.ptr = self.ptr.wrapping_sub(bytes);
            self.consumed = self
                .consumed
                .wrapping_sub(u32::try_from(bytes).unwrap_or(0).wrapping_mul(8));
        }
        self.container = load(self.data, self.ptr);
    }

    /// Refill for hot loops. If the container can move back over all its
    /// consumed whole bytes (at least eight more bytes precede it), does
    /// so, leaving at most 7 bits consumed (57 available), and returns the
    /// container and consumed count. Otherwise returns `None`.
    #[inline(always)]
    pub(super) fn refill_fast(&mut self) -> Option<(u64, u32)> {
        if self.ptr < 8 || self.consumed > 64 {
            return None;
        }
        self.ptr = self.ptr.wrapping_sub((self.consumed >> 3) as usize);
        self.consumed &= 7;
        self.container = load(self.data, self.ptr);
        Some((self.container, self.consumed))
    }

    /// Reads `n` bits (at most 57) without any check. Only valid right
    /// after a successful [`refill_fast`](Self::refill_fast), for reads
    /// totalling at most 57 bits.
    #[inline(always)]
    pub(super) fn read_unchecked(&mut self, n: u32) -> u64 {
        let value = ((self.container << (self.consumed & 63)) >> 1) >> (63u32.wrapping_sub(n) & 63);
        self.consumed = self.consumed.wrapping_add(n);
        value
    }

    /// Sets the consumed-bit count after a hot loop that worked on the
    /// values [`refill_fast`](Self::refill_fast) returned.
    #[inline(always)]
    pub(super) fn set_consumed(&mut self, consumed: u32) {
        self.consumed = consumed;
    }

    /// Returns the next `n` bits (at most 56) without consuming them.
    #[inline(always)]
    pub(super) fn peek(&mut self, n: u32) -> u64 {
        if self.consumed.wrapping_add(n) > 64 {
            self.reload();
        }
        if n == 0 || self.consumed >= 64 {
            return 0;
        }
        (self.container << self.consumed) >> (64u32.wrapping_sub(n))
    }

    /// Consumes `n` bits after a [`peek`](Self::peek).
    #[inline(always)]
    pub(super) fn consume(&mut self, n: u32) {
        self.consumed = self.consumed.saturating_add(n);
    }

    /// Reads `n` bits (at most 56).
    #[inline(always)]
    pub(super) fn read(&mut self, n: u32) -> u64 {
        let value = self.peek(n);
        self.consume(n);
        value
    }

    /// Whether more bits were read than the stream holds.
    #[inline(always)]
    pub(super) fn overflowed(&self) -> bool {
        self.consumed > 64
    }

    /// Whether every bit has been read, and no more.
    pub(super) fn finished(&self) -> bool {
        self.ptr == 0 && self.consumed == 64
    }
}

/// Reads bits forwards, least significant bit first, from a byte slice.
/// Bits past the end read as zero; the caller checks [`bytes_used`].
///
/// [`bytes_used`]: Self::bytes_used
pub(super) struct ForwardBits<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> ForwardBits<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    /// Returns the next `n` bits (at most 32) without consuming them.
    pub(super) fn peek(&self, n: u32) -> u32 {
        let byte = self.bit >> 3;
        let word = match self.data.get(byte..byte.wrapping_add(8)) {
            Some(bytes) => u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])),
            None => {
                let mut bytes = [0u8; 8];
                let tail = self.data.get(byte..).unwrap_or_default();
                if let Some(dst) = bytes.get_mut(..tail.len()) {
                    dst.copy_from_slice(tail);
                }
                u64::from_le_bytes(bytes)
            }
        };
        let shifted = word >> (self.bit & 7);
        (shifted & !u64::MAX.wrapping_shl(n)) as u32
    }

    pub(super) fn consume(&mut self, n: u32) {
        self.bit = self.bit.saturating_add(n as usize);
    }

    /// Whole bytes touched so far.
    pub(super) fn bytes_used(&self) -> usize {
        self.bit.div_ceil(8)
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    /// Writes bits the way a Zstandard encoder does: forwards, then a
    /// marker bit; a backward reader returns them last-written first.
    fn backward_stream(fields: &[(u64, u32)]) -> Vec<u8> {
        let mut acc: u128 = 0;
        let mut count = 0;
        let mut out = Vec::new();
        for &(value, bits) in fields.iter().chain(std::iter::once(&(1, 1))) {
            acc |= u128::from(value) << count;
            count += bits;
            while count >= 8 {
                out.push(acc as u8);
                acc >>= 8;
                count -= 8;
            }
        }
        if count > 0 {
            out.push(acc as u8);
        }
        out
    }

    #[test]
    fn backward_reads_in_reverse_order() {
        for n in [1usize, 3, 9, 40] {
            let fields: Vec<(u64, u32)> = (0..n)
                .map(|i| {
                    (
                        (i as u64 * 0x9e37_79b9) & ((1 << (i % 29 + 1)) - 1),
                        (i % 29 + 1) as u32,
                    )
                })
                .collect();
            let stream = backward_stream(&fields);
            let mut bits = BackwardBits::new(&stream).unwrap();
            for &(value, width) in fields.iter().rev() {
                assert_eq!(bits.read(width), value);
            }
            assert!(bits.finished(), "n = {n}");
            assert!(!bits.overflowed());
            bits.read(1);
            assert!(bits.overflowed());
        }
    }

    #[test]
    fn backward_rejects_missing_marker() {
        assert!(BackwardBits::new(&[]).is_err());
        assert!(BackwardBits::new(&[1, 0]).is_err());
    }

    #[test]
    fn forward_reads_lsb_first() {
        let mut bits = ForwardBits::new(&[0b1010_1100, 0xff]);
        assert_eq!(bits.peek(4), 0b1100);
        bits.consume(4);
        assert_eq!(bits.peek(8), 0xfa);
        bits.consume(12);
        assert_eq!(bits.bytes_used(), 2);
        assert_eq!(bits.peek(5), 0);
    }
}
