//! SHA-1, as specified by FIPS 180-4.

use super::{BLOCK, BlockBuffer};

/// Incremental SHA-1 hasher.
///
/// ```
/// use qld::output::hash::Sha1;
///
/// let mut hasher = Sha1::new();
/// hasher.update(b"a");
/// hasher.update(b"bc");
/// assert_eq!(hasher.finalize(), Sha1::digest(b"abc"));
/// ```
#[derive(Clone)]
pub struct Sha1 {
    state: [u32; 5],
    buffer: BlockBuffer,
}

impl Sha1 {
    /// Digest length in bytes.
    pub const LEN: usize = 20;

    /// Creates a hasher with the FIPS 180-4 initial state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: [
                0x6745_2301,
                0xefcd_ab89,
                0x98ba_dcfe,
                0x1032_5476,
                0xc3d2_e1f0,
            ],
            buffer: BlockBuffer::new(),
        }
    }

    /// Hashes `data` in one call.
    #[must_use]
    pub fn digest(data: &[u8]) -> [u8; Self::LEN] {
        let mut hasher = Self::new();
        hasher.update(data);
        hasher.finalize()
    }

    /// Appends `data` to the message.
    pub fn update(&mut self, data: &[u8]) {
        let state = &mut self.state;
        self.buffer
            .update(data, &mut |block| compress(state, block));
    }

    /// Pads the message and returns the digest.
    #[must_use]
    pub fn finalize(mut self) -> [u8; Self::LEN] {
        let length = self.buffer.bit_length().to_be_bytes();
        let state = &mut self.state;
        self.buffer
            .finish(length, &mut |block| compress(state, block));
        let mut out = [0u8; Self::LEN];
        for (dst, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *dst = word.to_be_bytes();
        }
        out
    }
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

/// One round group of [`compress`]: `$f` is the round function of `b`, `c`
/// and `d`, `$k` the constant.
macro_rules! rounds {
    ($w:expr, $k:expr, [$a:ident, $b:ident, $c:ident, $d:ident, $e:ident], $f:expr) => {
        for &word in $w {
            let temp = $a
                .rotate_left(5)
                .wrapping_add($f)
                .wrapping_add($e)
                .wrapping_add($k)
                .wrapping_add(word);
            $e = $d;
            $d = $c;
            $c = $b.rotate_left(30);
            $b = $a;
            $a = temp;
        }
    };
}

fn compress(state: &mut [u32; 5], block: &[u8; BLOCK]) {
    let mut w = [0u32; 80];
    for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_be_bytes(*bytes);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *state;
    // One loop per round function, with branch-free forms of Ch and Maj:
    // 30% faster than choosing the function inside a single loop.
    rounds!(&w[..20], 0x5a82_7999u32, [a, b, c, d, e], d ^ (b & (c ^ d)));
    rounds!(&w[20..40], 0x6ed9_eba1u32, [a, b, c, d, e], b ^ c ^ d);
    rounds!(
        &w[40..60],
        0x8f1b_bcdcu32,
        [a, b, c, d, e],
        (b & c) | (d & (b | c))
    );
    rounds!(&w[60..], 0xca62_c1d6u32, [a, b, c, d, e], b ^ c ^ d);
    for (st, v) in state.iter_mut().zip([a, b, c, d, e]) {
        *st = st.wrapping_add(v);
    }
}

#[cfg(test)]
mod tests {
    use super::Sha1;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Examples from FIPS 180 (the NIST "SHA-1 examples" document), plus
    /// the empty message and a well-known sentence.
    #[test]
    fn fips180_vectors() {
        let vectors: &[(&[u8], &str)] = &[
            (b"", "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
            (b"abc", "a9993e364706816aba3e25717850c26c9cd0d89d"),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
            ),
            (
                b"The quick brown fox jumps over the lazy dog",
                "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12",
            ),
        ];
        for (input, expected) in vectors {
            assert_eq!(hex(&Sha1::digest(input)), *expected);
        }
    }

    /// FIPS 180: one million repetitions of `a`, fed in uneven pieces.
    #[test]
    fn million_a() {
        let mut hasher = Sha1::new();
        let data = vec![b'a'; 1_000_000];
        let mut rest = &data[..];
        let mut step = 1;
        while !rest.is_empty() {
            let n = step.min(rest.len());
            hasher.update(&rest[..n]);
            rest = &rest[n..];
            step = step * 3 + 1;
        }
        assert_eq!(
            hex(&hasher.finalize()),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn padding_boundaries_match_one_shot() {
        let data: Vec<u8> = (0..300u32)
            .map(|i| (i.wrapping_mul(17) % 253) as u8)
            .collect();
        for len in [55, 56, 57, 63, 64, 65, 119, 120, 121, 128, 300] {
            let one_shot = Sha1::digest(&data[..len]);
            for split in [0, 1, 9, 56, 64, len / 2, len] {
                let mut hasher = Sha1::new();
                hasher.update(&data[..split.min(len)]);
                hasher.update(&data[split.min(len)..len]);
                assert_eq!(hasher.finalize(), one_shot, "len {len} split {split}");
            }
        }
    }
}
