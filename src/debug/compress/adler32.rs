//! Adler-32 (RFC 1950 section 8.2), with the combine operation zlib calls
//! `adler32_combine`.
//!
//! The checksum is two sums modulo 65521: `a` = 1 + the sum of the bytes,
//! `b` = the sum of the successive values of `a`. Bytes are summed in blocks
//! short enough that the 32-bit sums cannot overflow before the reduction
//! ([`NMAX`], the bound zlib uses).

/// The largest prime below 2^16.
const MOD: u32 = 65_521;

/// The most bytes that can be summed before `b` could overflow 32 bits,
/// starting from reduced sums: 255·n·(n+1)/2 + (n+1)·(MOD−1) < 2^32.
const NMAX: usize = 5552;

/// The Adler-32 of the empty string.
pub const ADLER32_INIT: u32 = 1;

/// Computes the Adler-32 of `data`.
///
/// ```
/// use qld::debug::compress::adler32;
/// assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
/// ```
#[must_use]
pub fn adler32(data: &[u8]) -> u32 {
    adler32_update(ADLER32_INIT, data)
}

/// Continues an Adler-32 computation: returns the checksum of the bytes
/// summarized by `adler` followed by `data`.
#[must_use]
pub fn adler32_update(adler: u32, data: &[u8]) -> u32 {
    let mut a = adler & 0xffff;
    let mut b = adler >> 16;
    for block in data.chunks(NMAX) {
        // Four bytes per step: each byte's contribution to `b` is its value
        // times the number of `a` updates that follow it in the step.
        let (quads, tail) = block.as_chunks::<4>();
        for q in quads {
            let [q0, q1, q2, q3] = q.map(u32::from);
            // Within a block, a < 2^16 + 4·255·NMAX and b grows by at most
            // 4·a + 10·255 per step; both stay below 2^32 (see NMAX).
            b = b
                .wrapping_add(a.wrapping_mul(4))
                .wrapping_add(q0.wrapping_mul(4))
                .wrapping_add(q1.wrapping_mul(3))
                .wrapping_add(q2.wrapping_mul(2))
                .wrapping_add(q3);
            a = a
                .wrapping_add(q0)
                .wrapping_add(q1)
                .wrapping_add(q2)
                .wrapping_add(q3);
        }
        for &byte in tail {
            a = a.wrapping_add(u32::from(byte));
            b = b.wrapping_add(a);
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Combines the Adler-32 checksums of two strings: given `adler1` of `A` and
/// `adler2` of `B` (where `B` is `len2` bytes long), returns the Adler-32 of
/// `A` followed by `B`.
///
/// This lets chunks compressed in parallel be checksummed independently.
///
/// ```
/// use qld::debug::compress::{adler32, adler32_combine};
/// let (a, b) = (b"hello, ".as_slice(), b"world".as_slice());
/// assert_eq!(
///     adler32_combine(adler32(a), adler32(b), b.len() as u64),
///     adler32(b"hello, world"),
/// );
/// ```
#[must_use]
pub fn adler32_combine(adler1: u32, adler2: u32, len2: u64) -> u32 {
    // Every value is reduced modulo MOD (< 2^16) before it is used, so the
    // few-term sums and the one product below fit easily in 64 bits.
    const M: u64 = MOD as u64;
    let rem = len2 % M;
    let a1 = u64::from(adler1 & 0xffff) % M;
    let b1 = u64::from(adler1 >> 16) % M;
    let a2 = u64::from(adler2 & 0xffff) % M;
    let b2 = u64::from(adler2 >> 16) % M;

    // a = a1 + a2 - 1
    // b = b1 + b2 + rem·a1 - rem: B's sums started from a = 1, but in the
    // concatenation each of B's len2 bytes also sees A's a1 - 1.
    let a = a1.wrapping_add(a2).wrapping_add(M.wrapping_sub(1)) % M;
    let b = b1
        .wrapping_add(b2)
        .wrapping_add(rem.wrapping_mul(a1) % M)
        .wrapping_add(M)
        .wrapping_sub(rem)
        % M;
    let a = u32::try_from(a).unwrap_or(0);
    let b = u32::try_from(b).unwrap_or(0);
    (b << 16) | a
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    fn reference(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in data {
            a = (a + u32::from(byte)) % MOD;
            b = (b + a) % MOD;
        }
        (b << 16) | a
    }

    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect()
    }

    #[test]
    fn known_values() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"a"), 0x0062_0062);
        assert_eq!(adler32(b"abc"), 0x024d_0127);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn matches_reference_across_block_boundaries() {
        for len in [0, 1, 3, 4, 5, 5551, 5552, 5553, 11_104, 70_001] {
            let data = noise(len, 0x9e37_79b9_7f4a_7c15 ^ len as u64);
            assert_eq!(adler32(&data), reference(&data), "len {len}");
            let ones = vec![0xffu8; len];
            assert_eq!(adler32(&ones), reference(&ones), "0xff len {len}");
        }
    }

    #[test]
    fn update_is_incremental() {
        let data = noise(20_000, 7);
        let (x, y) = data.split_at(7_777);
        assert_eq!(adler32_update(adler32(x), y), adler32(&data));
    }

    #[test]
    fn combine_matches_concatenation() {
        let data = noise(200_000, 99);
        for split in [0, 1, 2, 65_520, 65_521, 65_522, 131_042, 199_999, 200_000] {
            let (x, y) = data.split_at(split);
            assert_eq!(
                adler32_combine(adler32(x), adler32(y), y.len() as u64),
                adler32(&data),
                "split {split}"
            );
        }
        let ones = vec![0xffu8; 300_000];
        for split in [0, 65_521, 100_000, 300_000] {
            let (x, y) = ones.split_at(split);
            assert_eq!(
                adler32_combine(adler32(x), adler32(y), y.len() as u64),
                adler32(&ones),
                "0xff split {split}"
            );
        }
    }
}
