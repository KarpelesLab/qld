//! Small, dependency-free hash functions used for `--build-id`.
//!
//! - [`Md5`] (RFC 1321) and [`Sha1`] (FIPS 180-4) are the classic build-id
//!   digests. They are implemented here rather than pulled in as
//!   dependencies because they are short and have no performance-critical
//!   variants that matter at build-id sizes.
//! - [`xxh64`] is xxHash64, used for `--build-id=fast`. Its output is defined
//!   by the xxHash specification, so it is identical on every platform and
//!   will not change between qld releases.
//!
//! None of these are used for security. MD5 and SHA-1 are broken as
//! cryptographic hashes; for build-ids they only need to identify content.

mod md5;
mod sha1;
mod xxh64;

pub use md5::Md5;
pub use sha1::Sha1;
pub use xxh64::{Xxh64, xxh64};

/// Size of the blocks MD5 and SHA-1 compress.
const BLOCK: usize = 64;

/// The 64-byte block buffer and length counter shared by MD5 and SHA-1
/// (Merkle–Damgård construction with 64-bit length padding).
#[derive(Clone)]
struct BlockBuffer {
    buf: [u8; BLOCK],
    /// Bytes currently held in `buf`; always less than [`BLOCK`] between
    /// calls.
    len: usize,
    /// Total bytes hashed so far, modulo 2^64.
    total: u64,
}

impl BlockBuffer {
    const fn new() -> Self {
        Self {
            buf: [0; BLOCK],
            len: 0,
            total: 0,
        }
    }

    /// Feeds `data`, calling `compress` for every complete block.
    fn update(&mut self, data: &[u8], compress: &mut impl FnMut(&[u8; BLOCK])) {
        self.total = self.total.wrapping_add(data.len() as u64);
        self.feed(data, compress);
    }

    /// Like [`Self::update`] but without counting the bytes, for padding.
    fn feed(&mut self, mut data: &[u8], compress: &mut impl FnMut(&[u8; BLOCK])) {
        if self.len != 0 {
            if let Some(free) = self.buf.get_mut(self.len..) {
                let take = free.len().min(data.len());
                if let (Some(dst), Some((src, rest))) =
                    (free.get_mut(..take), data.split_at_checked(take))
                {
                    dst.copy_from_slice(src);
                    data = rest;
                    self.len = self.len.saturating_add(take);
                }
            }
            if self.len < BLOCK {
                return;
            }
            compress(&self.buf);
            self.len = 0;
        }
        let (blocks, rest) = data.as_chunks::<BLOCK>();
        for block in blocks {
            compress(block);
        }
        if let Some(dst) = self.buf.get_mut(..rest.len()) {
            dst.copy_from_slice(rest);
            self.len = rest.len();
        }
    }

    /// Appends the standard padding: `0x80`, zeros up to 56 bytes modulo 64,
    /// then the message length in bits as 8 bytes (`length` is already in
    /// the byte order the hash wants).
    fn finish(&mut self, length: [u8; 8], compress: &mut impl FnMut(&[u8; BLOCK])) {
        let mut pad = [0u8; BLOCK];
        pad[0] = 0x80;
        // 1..=64 bytes of padding so that `len` ends at 56.
        let pad_len = if self.len < 56 {
            56usize.saturating_sub(self.len)
        } else {
            120usize.saturating_sub(self.len)
        };
        self.feed(pad.get(..pad_len).unwrap_or(&pad), compress);
        self.feed(&length, compress);
    }

    /// Message length in bits, modulo 2^64.
    fn bit_length(&self) -> u64 {
        self.total.wrapping_mul(8)
    }
}
