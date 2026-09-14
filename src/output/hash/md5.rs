//! MD5, as specified by RFC 1321.

use super::{BLOCK, BlockBuffer};

/// Per-round additive constants: `floor(abs(sin(i + 1)) * 2^32)`.
const K: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// Per-round left-rotation amounts.
const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// Incremental MD5 hasher.
///
/// ```
/// use qld::output::hash::Md5;
///
/// let mut hasher = Md5::new();
/// hasher.update(b"message ");
/// hasher.update(b"digest");
/// assert_eq!(hasher.finalize(), Md5::digest(b"message digest"));
/// ```
#[derive(Clone)]
pub struct Md5 {
    state: [u32; 4],
    buffer: BlockBuffer,
}

impl Md5 {
    /// Digest length in bytes.
    pub const LEN: usize = 16;

    /// Creates a hasher with the RFC 1321 initial state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
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
        let length = self.buffer.bit_length().to_le_bytes();
        let state = &mut self.state;
        self.buffer
            .finish(length, &mut |block| compress(state, block));
        let mut out = [0u8; Self::LEN];
        for (dst, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *dst = word.to_le_bytes();
        }
        out
    }
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

fn compress(state: &mut [u32; 4], block: &[u8; BLOCK]) {
    let mut m = [0u32; 16];
    for (word, bytes) in m.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for (i, (&k, &s)) in K.iter().zip(S.iter()).enumerate() {
        let (f, g) = match i {
            0..16 => ((b & c) | (!b & d), i),
            16..32 => ((d & b) | (!d & c), i.wrapping_mul(5).wrapping_add(1)),
            32..48 => (b ^ c ^ d, i.wrapping_mul(3).wrapping_add(5)),
            _ => (c ^ (b | !d), i.wrapping_mul(7)),
        };
        let f = f.wrapping_add(a).wrapping_add(k).wrapping_add(m[g & 15]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(f.rotate_left(s));
    }
    for (st, v) in state.iter_mut().zip([a, b, c, d]) {
        *st = st.wrapping_add(v);
    }
}

#[cfg(test)]
mod tests {
    use super::Md5;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The test suite from RFC 1321, appendix A.5.
    #[test]
    fn rfc1321_vectors() {
        let vectors: &[(&[u8], &str)] = &[
            (b"", "d41d8cd98f00b204e9800998ecf8427e"),
            (b"a", "0cc175b9c0f1b6a831c399e269772661"),
            (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
            (b"message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                b"abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, expected) in vectors {
            assert_eq!(hex(&Md5::digest(input)), *expected);
        }
    }

    #[test]
    fn padding_boundaries_match_one_shot() {
        let data: Vec<u8> = (0..300u32)
            .map(|i| (i.wrapping_mul(31) % 251) as u8)
            .collect();
        for len in [55, 56, 57, 63, 64, 65, 119, 120, 121, 128, 300] {
            let one_shot = Md5::digest(&data[..len]);
            for split in [0, 1, 7, 55, 64, len / 2, len] {
                let mut hasher = Md5::new();
                hasher.update(&data[..split.min(len)]);
                hasher.update(&data[split.min(len)..len]);
                assert_eq!(hasher.finalize(), one_shot, "len {len} split {split}");
            }
        }
    }

    #[test]
    fn million_a() {
        let mut hasher = Md5::new();
        for _ in 0..1000 {
            hasher.update(&[b'a'; 1000]);
        }
        assert_eq!(hex(&hasher.finalize()), "7707d6ae4e027c70eea2a935c2296f21");
    }
}
