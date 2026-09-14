//! xxHash64 (XXH64), as specified in the xxHash specification
//! (`doc/xxhash_spec.md` in the reference repository).
//!
//! All arithmetic is wrapping and all multi-byte reads are little-endian, so
//! the result is the same on every platform.

const P1: u64 = 0x9e37_79b1_85eb_ca87;
const P2: u64 = 0xc2b2_ae3d_27d4_eb4f;
const P3: u64 = 0x1656_67b1_9e37_79f9;
const P4: u64 = 0x85eb_ca77_c2b2_ae63;
const P5: u64 = 0x27d4_eb2f_1656_67c5;

#[inline]
fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

#[inline]
fn merge_round(acc: u64, value: u64) -> u64 {
    (acc ^ round(0, value)).wrapping_mul(P1).wrapping_add(P4)
}

/// Computes the 64-bit xxHash of `data` with the given `seed`.
///
/// The value is fixed by the xxHash specification; to store it as bytes, use
/// the canonical big-endian form (`to_be_bytes`), which is what `xxhsum`
/// prints.
///
/// ```
/// assert_eq!(qld::output::hash::xxh64(b"abc", 0), 0x44bc_2cf5_ad77_0999);
/// ```
#[must_use]
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let (stripes, mut rest) = data.as_chunks::<32>();
    let mut hash = if stripes.is_empty() {
        seed.wrapping_add(P5)
    } else {
        let mut v = [
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        ];
        for stripe in stripes {
            for (acc, lane) in v.iter_mut().zip(stripe.as_chunks::<8>().0) {
                *acc = round(*acc, u64::from_le_bytes(*lane));
            }
        }
        let [v1, v2, v3, v4] = v;
        let mut hash = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        for lane in v {
            hash = merge_round(hash, lane);
        }
        hash
    };
    hash = hash.wrapping_add(data.len() as u64);

    let (words, tail) = rest.as_chunks::<8>();
    for word in words {
        hash ^= round(0, u64::from_le_bytes(*word));
        hash = hash.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
    }
    rest = tail;
    let (halves, tail) = rest.as_chunks::<4>();
    for half in halves {
        hash ^= u64::from(u32::from_le_bytes(*half)).wrapping_mul(P1);
        hash = hash.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
    }
    for &byte in tail {
        hash ^= u64::from(byte).wrapping_mul(P5);
        hash = hash.rotate_left(11).wrapping_mul(P1);
    }

    hash ^= hash >> 33;
    hash = hash.wrapping_mul(P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(P3);
    hash ^= hash >> 32;
    hash
}

#[cfg(test)]
mod tests {
    use super::xxh64;

    /// The reference implementation's sanity buffer: a byte generator
    /// seeded with PRIME32, multiplied by PRIME64 at every step.
    fn sanity_buffer() -> Vec<u8> {
        let mut generator: u64 = 2_654_435_761;
        (0..2367)
            .map(|_| {
                let byte = (generator >> 56) as u8;
                generator = generator.wrapping_mul(11_400_714_785_074_694_797);
                byte
            })
            .collect()
    }

    /// Vectors from the reference implementation's sanity checks
    /// (`xxhsum -b` self-test); the lengths not in that list were checked
    /// against `xxhsum` 0.8.3.
    #[test]
    fn reference_sanity_vectors() {
        let buf = sanity_buffer();
        let prime = 2_654_435_761;
        let vectors: &[(usize, u64, u64)] = &[
            (0, 0, 0xef46_db37_51d8_e999),
            (0, prime, 0xac75_fda2_929b_17ef),
            (1, 0, 0xe934_a84a_db05_2768),
            (1, prime, 0x5014_6076_43a9_b4c3),
            (4, 0, 0x9136_a0dc_a574_57ee),
            (8, 0, 0xcdbc_f538_e71d_1348),
            (14, 0, 0x8282_dcc4_994e_35c8),
            (14, prime, 0xc3bd_6bf6_3deb_6df0),
            (32, 0, 0x18b2_1649_2bb4_4b70),
            (63, 0, 0xa9ef_be0f_a0f3_f4e7),
            (64, 0, 0xef55_8f8a_cac2_b5cd),
            (222, 0, 0xb641_ae8c_b691_c174),
            (222, prime, 0x20cb_8ab7_ae10_c14a),
            (2367, 0, 0xa824_18dd_ec0e_a581),
        ];
        for &(len, seed, expected) in vectors {
            assert_eq!(xxh64(&buf[..len], seed), expected, "len {len} seed {seed}");
        }
    }

    #[test]
    fn string_vectors() {
        assert_eq!(xxh64(b"abc", 0), 0x44bc_2cf5_ad77_0999);
        assert_eq!(
            xxh64(b"Nobody inspects the spammish repetition", 0),
            0xfbce_a83c_8a37_8bf1
        );
        assert_eq!(xxh64(&[42], 0), 0x0a9e_dece_beb0_3ae4);
        assert_eq!(xxh64(b"Hello, world!\0", 0), 0x7b06_c531_ea43_e89f);
        let counting: Vec<u8> = (0..100u8).collect();
        assert_eq!(xxh64(&counting, 0), 0x6ac1_e580_3216_6597);
        assert_eq!(xxh64(&[], 0xae05_4331_1b70_2d91), 0x4b6a_04fc_df7a_4672);
        assert_eq!(
            xxh64(&counting, 0xae05_4331_1b70_2d91),
            0x567e_355e_0682_e1f1
        );
    }
}
