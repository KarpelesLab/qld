//! Ad-hoc code signatures (`LC_CODE_SIGNATURE`).
//!
//! arm64 macOS refuses to run unsigned code, so ld64 signs every arm64
//! output "ad hoc": a `SuperBlob` holding one `CodeDirectory` with a SHA-256
//! hash of each 4 KiB page of the file up to the signature, flagged
//! `CS_ADHOC | CS_LINKER_SIGNED` so that `codesign` may replace it. There is
//! no certificate and no requirements blob. All fields are big-endian.
//!
//! The signature must be the last thing in `__LINKEDIT`, and its size
//! depends only on the file offset it starts at and the identifier, so the
//! layout reserves [`signature_size`] bytes and [`write_signature`] fills
//! them once every other byte of the file is final.

#![deny(clippy::arithmetic_side_effects)]

use rayon::prelude::*;

use super::sha256::Sha256;
use crate::error::{Error, Result};

/// `CSMAGIC_EMBEDDED_SIGNATURE`.
pub const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
/// `CSMAGIC_CODEDIRECTORY`.
pub const CSMAGIC_CODEDIRECTORY: u32 = 0xfade_0c02;
/// `CSSLOT_CODEDIRECTORY`.
pub const CSSLOT_CODEDIRECTORY: u32 = 0;
/// `CS_ADHOC`.
pub const CS_ADHOC: u32 = 0x0000_0002;
/// `CS_LINKER_SIGNED`.
pub const CS_LINKER_SIGNED: u32 = 0x0002_0000;
/// `CS_HASHTYPE_SHA256`.
pub const CS_HASHTYPE_SHA256: u8 = 2;
/// `CS_EXECSEG_MAIN_BINARY`.
pub const CS_EXECSEG_MAIN_BINARY: u64 = 1;
/// The `CodeDirectory` version with the exec segment fields.
pub const CD_VERSION: u32 = 0x0002_0400;
/// log2 of the hashed page size.
pub const PAGE_SHIFT: u8 = 12;
/// The hashed page size.
pub const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;

/// The `SuperBlob` header and its one `BlobIndex`, padded to 8 bytes.
const BLOB_HEADERS: u64 = 24;
/// Size of a version 0x20400 `CodeDirectory` header.
const CODE_DIRECTORY_HEADER: u64 = 88;
const HASH_SIZE: u64 = 32;

/// What the signature covers.
#[derive(Clone, Debug)]
pub struct SignatureInput<'a> {
    /// The identifier, usually the output's file name.
    pub identifier: &'a [u8],
    /// File offset where the signature starts; everything before it is
    /// hashed.
    pub code_limit: u64,
    /// File offset of the `__TEXT` segment.
    pub exec_seg_base: u64,
    /// File size of the `__TEXT` segment.
    pub exec_seg_limit: u64,
    /// Whether the file is a main executable (`CS_EXECSEG_MAIN_BINARY`).
    pub main_binary: bool,
}

fn page_count(code_limit: u64) -> u64 {
    code_limit.div_ceil(PAGE_SIZE)
}

/// Offset of the page hashes from the start of the signature: the headers
/// and the NUL-terminated identifier, padded to 16 bytes (as lld and
/// `llvm-objcopy` lay it out).
fn offset_of_hashes(identifier: &[u8]) -> u64 {
    BLOB_HEADERS
        .saturating_add(CODE_DIRECTORY_HEADER)
        .saturating_add(u64::try_from(identifier.len()).unwrap_or(u64::MAX))
        .saturating_add(1)
        .next_multiple_of(16)
}

/// The number of bytes the signature occupies. The signature itself starts
/// at a 16-byte boundary.
#[must_use]
pub fn signature_size(identifier: &[u8], code_limit: u64) -> u64 {
    offset_of_hashes(identifier).saturating_add(page_count(code_limit).saturating_mul(HASH_SIZE))
}

fn put32(out: &mut [u8], at: u64, value: u32) -> Option<()> {
    let at = usize::try_from(at).ok()?;
    out.get_mut(at..at.checked_add(4)?)?
        .copy_from_slice(&value.to_be_bytes());
    Some(())
}

fn put64(out: &mut [u8], at: u64, value: u64) -> Option<()> {
    let at = usize::try_from(at).ok()?;
    out.get_mut(at..at.checked_add(8)?)?
        .copy_from_slice(&value.to_be_bytes());
    Some(())
}

/// Writes the signature into `file`, whose bytes before
/// `input.code_limit` must be final. The signature occupies
/// [`signature_size`] bytes starting at `code_limit`.
///
/// # Errors
///
/// [`Error::Internal`] if `file` is too short for the signature.
pub fn write_signature(file: &mut [u8], input: &SignatureInput<'_>) -> Result<()> {
    let bad = || Error::Internal("code signature does not fit the output".into());
    let size = signature_size(input.identifier, input.code_limit);
    let start = usize::try_from(input.code_limit).map_err(|_| bad())?;
    let end =
        usize::try_from(input.code_limit.checked_add(size).ok_or_else(bad)?).map_err(|_| bad())?;
    if end > file.len() {
        return Err(bad());
    }
    let (code, rest) = file.split_at_mut(start);
    let blob = rest.get_mut(..end.saturating_sub(start)).ok_or_else(bad)?;
    blob.fill(0);

    let pages = page_count(input.code_limit);
    let hashes_at = offset_of_hashes(input.identifier);
    let directory_length = size.saturating_sub(BLOB_HEADERS);
    let cd = BLOB_HEADERS;
    let u32_of = |v: u64| u32::try_from(v).map_err(|_| bad());

    // SuperBlob.
    put32(blob, 0, CSMAGIC_EMBEDDED_SIGNATURE).ok_or_else(bad)?;
    put32(blob, 4, u32_of(size)?).ok_or_else(bad)?;
    put32(blob, 8, 1).ok_or_else(bad)?;
    put32(blob, 12, CSSLOT_CODEDIRECTORY).ok_or_else(bad)?;
    put32(blob, 16, u32_of(cd)?).ok_or_else(bad)?;

    // CodeDirectory, offsets relative to its start.
    let rel = |at: u64| cd.saturating_add(at);
    put32(blob, rel(0), CSMAGIC_CODEDIRECTORY).ok_or_else(bad)?;
    put32(blob, rel(4), u32_of(directory_length)?).ok_or_else(bad)?;
    put32(blob, rel(8), CD_VERSION).ok_or_else(bad)?;
    put32(blob, rel(12), CS_ADHOC | CS_LINKER_SIGNED).ok_or_else(bad)?;
    put32(blob, rel(16), u32_of(hashes_at.saturating_sub(cd))?).ok_or_else(bad)?;
    put32(blob, rel(20), u32_of(CODE_DIRECTORY_HEADER)?).ok_or_else(bad)?;
    put32(blob, rel(24), 0).ok_or_else(bad)?; // nSpecialSlots
    put32(blob, rel(28), u32_of(pages)?).ok_or_else(bad)?;
    put32(blob, rel(32), u32_of(input.code_limit)?).ok_or_else(bad)?;
    {
        let at = usize::try_from(rel(36)).map_err(|_| bad())?;
        let header = blob.get_mut(at..at.saturating_add(4)).ok_or_else(bad)?;
        // hashSize, hashType, platform, pageSize.
        header.copy_from_slice(&[32, CS_HASHTYPE_SHA256, 0, PAGE_SHIFT]);
    }
    // spare2 (40), scatterOffset (44), teamOffset (48), spare3 (52) and
    // codeLimit64 (56) stay zero.
    put64(blob, rel(64), input.exec_seg_base).ok_or_else(bad)?;
    put64(blob, rel(72), input.exec_seg_limit).ok_or_else(bad)?;
    let flags = if input.main_binary {
        CS_EXECSEG_MAIN_BINARY
    } else {
        0
    };
    put64(blob, rel(80), flags).ok_or_else(bad)?;
    let ident_at = usize::try_from(rel(CODE_DIRECTORY_HEADER)).map_err(|_| bad())?;
    blob.get_mut(ident_at..ident_at.saturating_add(input.identifier.len()))
        .ok_or_else(bad)?
        .copy_from_slice(input.identifier);

    let hashes_start = usize::try_from(hashes_at).map_err(|_| bad())?;
    let hashes_len = usize::try_from(pages.saturating_mul(HASH_SIZE)).map_err(|_| bad())?;
    let hashes = blob
        .get_mut(hashes_start..hashes_start.saturating_add(hashes_len))
        .ok_or_else(bad)?;
    hashes
        .par_chunks_mut(32)
        .zip(code.par_chunks(4096))
        .for_each(|(slot, page)| slot.copy_from_slice(&Sha256::digest(page)));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_hashes() {
        let code_limit = 10_000u64;
        let identifier = b"a.out";
        let size = signature_size(identifier, code_limit);
        let mut file: Vec<u8> = (0..code_limit).map(|i| (i % 253) as u8).collect();
        file.resize((code_limit + size) as usize, 0xaa);
        write_signature(
            &mut file,
            &SignatureInput {
                identifier,
                code_limit,
                exec_seg_base: 0,
                exec_seg_limit: 0x4000,
                main_binary: true,
            },
        )
        .unwrap();
        let sig = &file[code_limit as usize..];
        let be = |at: usize| u32::from_be_bytes(sig[at..at + 4].try_into().unwrap());
        assert_eq!(be(0), CSMAGIC_EMBEDDED_SIGNATURE);
        assert_eq!(be(4) as u64, size);
        assert_eq!(be(24), CSMAGIC_CODEDIRECTORY);
        let hash_offset = 24 + be(24 + 16) as usize;
        assert_eq!(hash_offset % 16, 0);
        assert_eq!(be(24 + 28), 3); // pages
        for page in 0..3 {
            let start = page * 4096;
            let end = (start + 4096).min(code_limit as usize);
            assert_eq!(
                &sig[hash_offset + page * 32..hash_offset + page * 32 + 32],
                &Sha256::digest(&file[start..end])
            );
        }
        assert_eq!(&sig[24 + 88..24 + 88 + 5], b"a.out");
        assert_eq!(size as usize, hash_offset + 3 * 32);
    }
}
