//! Build-id computation (`--build-id`).
//!
//! # Tree hashing
//!
//! Hashing a large output serially is a noticeable part of link time, so the
//! content-derived modes hash in two levels:
//!
//! 1. Split the image into [`BLOCK_SIZE`] (1 MiB) blocks; the last one may be
//!    shorter. Hash every block in parallel with the mode's hash function.
//! 2. Hash the concatenation of the block digests, in block order, with the
//!    same function. That digest is the build-id.
//!
//! Block boundaries depend only on the image size, so the result is
//! independent of the thread count. It is not the plain MD5 or SHA-1 of the
//! file (GNU ld hashes serially; lld also uses a tree, with its own block
//! layout), which is fine: a build-id only has to identify the content. The
//! block size and the digest encodings below are part of the output format;
//! changing either changes every build-id qld produces.
//!
//! | Mode | Hash | Size |
//! | --- | --- | --- |
//! | `fast` | xxHash64, seed 0, digests as 8 big-endian bytes | 8 |
//! | `md5` | MD5 | 16 |
//! | `sha1` | SHA-1 | 20 |
//! | `uuid` | random version 4 UUID, see [`random_uuid`] | 16 |
//! | `0x…` | the given bytes | any |
//!
//! # Hashing in place
//!
//! The build-id note lives inside the image it identifies. Layout reserves
//! [`build_id_size`] bytes for it; after everything else is written,
//! [`apply_build_id`] zeroes that field, hashes the image, and writes the
//! digest into the field. Zeroing first makes the result independent of
//! whatever the field held before.

#![deny(clippy::arithmetic_side_effects)]

use super::chunks::{ChunkRange, LayoutError};
use super::hash::{Md5, Sha1, xxh64};
pub use super::random::random_uuid;
use crate::args::BuildId;
use rayon::prelude::*;

/// Size of the blocks hashed in parallel: 1 MiB.
pub const BLOCK_SIZE: usize = 1 << 20;

/// Number of bytes the build-id for `kind` occupies, or `None` for
/// [`BuildId::None`].
#[must_use]
pub fn build_id_size(kind: &BuildId) -> Option<usize> {
    match kind {
        BuildId::None => None,
        BuildId::Fast => Some(8),
        BuildId::Md5 | BuildId::Uuid => Some(Md5::LEN),
        BuildId::Sha1 => Some(Sha1::LEN),
        BuildId::Hex(bytes) => Some(bytes.len()),
    }
}

/// Computes the build-id of `image` as is (without zeroing any field).
///
/// Content-derived modes hash in parallel on the current rayon pool. Returns
/// `None` for [`BuildId::None`].
#[must_use]
pub fn compute_build_id(kind: &BuildId, image: &[u8]) -> Option<Vec<u8>> {
    match kind {
        BuildId::None => None,
        BuildId::Fast => Some(tree_hash(image, |data| xxh64(data, 0).to_be_bytes()).to_vec()),
        BuildId::Md5 => Some(tree_hash(image, Md5::digest).to_vec()),
        BuildId::Sha1 => Some(tree_hash(image, Sha1::digest).to_vec()),
        BuildId::Uuid => Some(random_uuid().to_vec()),
        BuildId::Hex(bytes) => Some(bytes.clone()),
    }
}

/// Zeroes the build-id field at `offset`, computes the build-id of the whole
/// image, writes it into the field, and returns it.
///
/// The field is [`build_id_size`]`(kind)` bytes long. Returns `Ok(None)`
/// without touching the image for [`BuildId::None`].
///
/// # Errors
///
/// Returns [`LayoutError::FieldOutOfBounds`] if the field does not fit in
/// the image. The image is unchanged in that case.
pub fn apply_build_id(
    kind: &BuildId,
    image: &mut [u8],
    offset: u64,
) -> Result<Option<Vec<u8>>, LayoutError> {
    let Some(size) = build_id_size(kind) else {
        return Ok(None);
    };
    let range = ChunkRange::new(offset, size as u64);
    let len = image.len() as u64;
    let out_of_bounds = || LayoutError::FieldOutOfBounds { range, len };
    let start = usize::try_from(offset).map_err(|_| out_of_bounds())?;
    let end = start.checked_add(size).ok_or_else(out_of_bounds)?;
    image.get_mut(start..end).ok_or_else(out_of_bounds)?.fill(0);
    let Some(id) = compute_build_id(kind, image) else {
        return Ok(None);
    };
    if let Some(field) = image.get_mut(start..end)
        && field.len() == id.len()
    {
        field.copy_from_slice(&id);
    }
    Ok(Some(id))
}

/// Two-level parallel hash: `leaf` over every block, then `leaf` over the
/// concatenated digests.
fn tree_hash<const N: usize>(image: &[u8], leaf: impl Fn(&[u8]) -> [u8; N] + Sync) -> [u8; N] {
    let digests: Vec<[u8; N]> = image.par_chunks(BLOCK_SIZE).map(&leaf).collect();
    leaf(digests.as_flattened())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(build_id_size(&BuildId::None), None);
        assert_eq!(build_id_size(&BuildId::Fast), Some(8));
        assert_eq!(build_id_size(&BuildId::Md5), Some(16));
        assert_eq!(build_id_size(&BuildId::Sha1), Some(20));
        assert_eq!(build_id_size(&BuildId::Uuid), Some(16));
        assert_eq!(build_id_size(&BuildId::Hex(vec![1, 2, 3])), Some(3));
    }

    #[test]
    fn tree_layout_is_hash_of_block_hashes() {
        let image: Vec<u8> = (0..(BLOCK_SIZE * 2 + 5)).map(|i| (i % 251) as u8).collect();
        let mut concat = Vec::new();
        for block in image.chunks(BLOCK_SIZE) {
            concat.extend_from_slice(&Sha1::digest(block));
        }
        assert_eq!(
            compute_build_id(&BuildId::Sha1, &image).unwrap(),
            Sha1::digest(&concat)
        );
        let mut concat = Vec::new();
        for block in image.chunks(BLOCK_SIZE) {
            concat.extend_from_slice(&xxh64(block, 0).to_be_bytes());
        }
        assert_eq!(
            compute_build_id(&BuildId::Fast, &image).unwrap(),
            xxh64(&concat, 0).to_be_bytes()
        );
    }

    /// Pins the encoding: changing the block size or digest layout must
    /// be a deliberate, visible change.
    #[test]
    fn empty_image_values_are_stable() {
        // Zero blocks: the hash of the empty concatenation.
        assert_eq!(
            compute_build_id(&BuildId::Fast, &[]).unwrap(),
            0xef46_db37_51d8_e999u64.to_be_bytes()
        );
        assert_eq!(
            compute_build_id(&BuildId::Md5, &[]).unwrap(),
            Md5::digest(b"")
        );
    }

    #[test]
    fn apply_zeroes_field_before_hashing() {
        let mut a = vec![7u8; 4096];
        let mut b = a.clone();
        b[100..120].fill(0xee);
        let id_a = apply_build_id(&BuildId::Sha1, &mut a, 100)
            .unwrap()
            .unwrap();
        let id_b = apply_build_id(&BuildId::Sha1, &mut b, 100)
            .unwrap()
            .unwrap();
        assert_eq!(id_a, id_b);
        assert_eq!(a, b);
        assert_eq!(&a[100..120], &id_a[..]);

        let mut expected = vec![7u8; 4096];
        expected[100..120].fill(0);
        assert_eq!(compute_build_id(&BuildId::Sha1, &expected).unwrap(), id_a);
    }

    #[test]
    fn apply_hex_and_none() {
        let mut image = vec![0u8; 16];
        let id = apply_build_id(&BuildId::Hex(vec![0xde, 0xad]), &mut image, 14).unwrap();
        assert_eq!(id, Some(vec![0xde, 0xad]));
        assert_eq!(&image[14..], [0xde, 0xad]);
        assert_eq!(apply_build_id(&BuildId::None, &mut image, 1000), Ok(None));
    }

    #[test]
    fn apply_rejects_out_of_bounds_fields() {
        let mut image = vec![1u8; 32];
        for offset in [13, 32, u64::MAX] {
            assert!(matches!(
                apply_build_id(&BuildId::Sha1, &mut image, offset),
                Err(LayoutError::FieldOutOfBounds { .. })
            ));
        }
        assert_eq!(image, vec![1u8; 32]);
    }
}
