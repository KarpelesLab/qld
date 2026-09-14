//! Finite State Entropy tables (RFC 8878 section 4.1).

use super::bits::ForwardBits;

/// Largest accuracy log any Zstandard FSE table uses (literal and match
/// lengths).
pub(super) const MAX_LOG: u32 = 9;
const MAX_TABLE: usize = 1 << MAX_LOG;

/// One decoding state: the symbol it emits, and how to reach the next state
/// (`base` + the next `bits` bits).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) symbol: u8,
    pub(super) bits: u8,
    pub(super) base: u16,
}

/// A decoding table of `1 << log` states.
#[derive(Clone)]
pub(super) struct Table {
    pub(super) log: u32,
    pub(super) entries: [Entry; MAX_TABLE],
}

impl Table {
    pub(super) const fn new() -> Self {
        Self {
            log: 0,
            entries: [Entry {
                symbol: 0,
                bits: 0,
                base: 0,
            }; MAX_TABLE],
        }
    }

    /// The state reached from `state`, masked into the table.
    #[inline(always)]
    pub(super) fn get(&self, state: u64) -> Entry {
        self.entries[(state as usize) & (MAX_TABLE - 1)]
    }

    /// A one-state table that always emits `symbol` and reads no bits
    /// (`RLE_Mode`).
    pub(super) fn rle(&mut self, symbol: u8) {
        self.log = 0;
        self.entries[0] = Entry {
            symbol,
            bits: 0,
            base: 0,
        };
    }

    /// Builds the table for normalized counts `norm` (`-1` meaning "less
    /// than one") at accuracy `log`.
    pub(super) fn build(&mut self, norm: &[i16], log: u32) -> Result<(), &'static str> {
        const BAD: &str = "zstd FSE table";
        if log > MAX_LOG {
            return Err(BAD);
        }
        let size = 1usize << log;
        let mask = size.wrapping_sub(1);
        let mut high = size.wrapping_sub(1);
        let mut next = [0u16; 256];
        if norm.len() > next.len() {
            return Err(BAD);
        }
        let total: i64 = norm.iter().map(|&c| i64::from(c).abs()).sum();
        if total != i64::try_from(size).unwrap_or(0) || norm.iter().any(|&c| c < -1) {
            return Err(BAD);
        }

        // Low-probability symbols take the last states.
        for (symbol, &count) in norm.iter().enumerate() {
            let slot = next.get_mut(symbol).ok_or(BAD)?;
            if count == -1 {
                let entry = self.entries.get_mut(high).ok_or(BAD)?;
                entry.symbol = symbol as u8;
                high = high.checked_sub(1).ok_or(BAD)?;
                *slot = 1;
            } else {
                *slot = u16::try_from(count).map_err(|_| BAD)?;
            }
        }

        // Spread the other symbols over the remaining states.
        let step = (size >> 1).wrapping_add(size >> 3).wrapping_add(3);
        let mut position = 0usize;
        for (symbol, &count) in norm.iter().enumerate() {
            for _ in 0..count.max(0) {
                let entry = self.entries.get_mut(position).ok_or(BAD)?;
                entry.symbol = symbol as u8;
                position = position.wrapping_add(step) & mask;
                while position > high {
                    position = position.wrapping_add(step) & mask;
                }
            }
        }
        if position != 0 {
            return Err(BAD);
        }

        for entry in self.entries.iter_mut().take(size) {
            let slot = next.get_mut(usize::from(entry.symbol)).ok_or(BAD)?;
            let state = *slot;
            *slot = state.wrapping_add(1);
            if state == 0 {
                return Err(BAD);
            }
            let high_bit = 15u32.wrapping_sub(state.leading_zeros());
            let bits = log.checked_sub(high_bit).ok_or(BAD)?;
            entry.bits = bits as u8;
            let base = u32::from(state)
                .wrapping_shl(bits)
                .checked_sub(u32::try_from(size).unwrap_or(0))
                .ok_or(BAD)?;
            entry.base = u16::try_from(base).map_err(|_| BAD)?;
        }
        self.log = log;
        Ok(())
    }
}

/// Reads an FSE table description (RFC 8878 section 4.1.1) from the start
/// of `data` into `norm`. Returns the bytes used, the accuracy log and the
/// number of symbols described.
pub(super) fn read_ncount(
    data: &[u8],
    max_symbol: usize,
    max_log: u32,
    norm: &mut [i16; 256],
) -> Result<(usize, u32, usize), &'static str> {
    const BAD: &str = "zstd FSE table description";
    let mut bits = ForwardBits::new(data);
    let log = bits.peek(4).wrapping_add(5);
    bits.consume(4);
    if log > max_log {
        return Err(BAD);
    }
    let mut remaining: i32 = (1i32 << log).wrapping_add(1);
    let mut threshold: i32 = 1i32 << log;
    let mut nb_bits = log.wrapping_add(1);
    let mut symbol = 0usize;
    let mut previous_zero = false;
    norm.fill(0);

    while remaining > 1 && symbol <= max_symbol {
        if previous_zero {
            // Runs of zero-probability symbols: 2-bit repeat flags, where 3
            // means "three more, and another flag follows".
            let mut run_end = symbol;
            loop {
                let flag = bits.peek(2);
                bits.consume(2);
                run_end = run_end.wrapping_add(flag as usize);
                if flag != 3 {
                    break;
                }
                if run_end > max_symbol || bits.bytes_used() > data.len() {
                    return Err(BAD);
                }
            }
            if run_end > max_symbol {
                return Err(BAD);
            }
            symbol = run_end;
        }

        let max = threshold
            .wrapping_mul(2)
            .wrapping_sub(1)
            .wrapping_sub(remaining);
        let low = bits.peek(nb_bits) as i32;
        let mut count;
        if low & threshold.wrapping_sub(1) < max {
            count = low & threshold.wrapping_sub(1);
            bits.consume(nb_bits.wrapping_sub(1));
        } else {
            count = low & threshold.wrapping_mul(2).wrapping_sub(1);
            if count >= threshold {
                count = count.wrapping_sub(max);
            }
            bits.consume(nb_bits);
        }
        count = count.wrapping_sub(1);
        remaining = remaining.wrapping_sub(count.abs());
        *norm.get_mut(symbol).ok_or(BAD)? = i16::try_from(count).map_err(|_| BAD)?;
        symbol = symbol.wrapping_add(1);
        previous_zero = count == 0;
        while remaining < threshold && nb_bits > 1 {
            nb_bits = nb_bits.wrapping_sub(1);
            threshold >>= 1;
        }
        if bits.bytes_used() > data.len() {
            return Err(BAD);
        }
    }
    if remaining != 1 {
        return Err(BAD);
    }
    Ok((bits.bytes_used(), log, symbol))
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn builds_predefined_literal_length_table() {
        // RFC 8878 section 3.1.1.3.2.2.
        let norm: [i16; 36] = [
            4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1,
            1, 1, 1, -1, -1, -1, -1,
        ];
        let mut table = Table::new();
        table.build(&norm, 6).unwrap();
        // Every symbol appears as often as its count says, and every state
        // leads back into the table.
        for (symbol, &count) in norm.iter().enumerate() {
            let n = table.entries[..64]
                .iter()
                .filter(|e| usize::from(e.symbol) == symbol)
                .count();
            assert_eq!(n as i16, count.max(1), "symbol {symbol}");
        }
        for e in &table.entries[..64] {
            assert!(usize::from(e.base) + (1 << e.bits) <= 64);
        }
        // Values from the RFC's decoding table (Appendix A).
        assert_eq!(
            table.entries[0],
            Entry {
                symbol: 0,
                bits: 4,
                base: 0
            }
        );
        assert_eq!(
            table.entries[1],
            Entry {
                symbol: 0,
                bits: 4,
                base: 16
            }
        );
        assert_eq!(
            table.entries[2],
            Entry {
                symbol: 1,
                bits: 5,
                base: 32
            }
        );
        assert_eq!(
            table.entries[63],
            Entry {
                symbol: 32,
                bits: 6,
                base: 0
            }
        );
    }

    #[test]
    fn rejects_invalid_counts() {
        let mut table = Table::new();
        // Too few states assigned: the spread does not return to 0.
        assert!(table.build(&[3, 3], 2).is_err());
    }
}
