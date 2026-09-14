//! Zstandard decompression (stub).

use super::DecodeError;

/// Decompresses Zstandard data into `out`.
///
/// # Errors
///
/// Always fails for now.
pub fn zstd_decompress_into(_input: &[u8], _out: &mut [u8]) -> Result<(), DecodeError> {
    Err(DecodeError::new(0, "zstd (not implemented)"))
}
