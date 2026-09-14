//! Raw image outputs: `--oformat binary`, `ihex` and `srec`.
//!
//! The link is laid out and rendered exactly as for ELF output, in memory;
//! the image is then cut into the pieces GNU ld hands to BFD's writers: for
//! every allocated output section with contents, each input section, data
//! command and padding run, at its load address (LMA). BFD's rules follow.
//!
//! - **binary**: every piece at `LMA - lowest LMA` of the loadable sections
//!   with contents; the file ends with the last piece, and gaps between
//!   sections are zeros (padding inside a section uses its fill).
//! - **ihex**: pieces sorted by address, each cut into 16-byte data records
//!   that do not cross 64 KiB boundaries; an extended segment address
//!   record (type 02) switches the base while every address fits 20 bits,
//!   then extended linear address records (type 04); a start address record
//!   (03 or 05) when the entry point is not 0; the end record.
//! - **srec**: an `S0` header holding the output file name (up to 40
//!   bytes), 16-byte data records whose type (`S1`, `S2` or `S3`) is the
//!   smallest that fits the highest address, and the matching `S9`, `S8` or
//!   `S7` termination record holding the entry point.

#![deny(clippy::arithmetic_side_effects)]

use crate::error::{Error, Result};

use super::layout::{Layout, Member, Trailer};

/// A raw output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `binary`.
    Binary,
    /// `ihex`.
    Ihex,
    /// `srec`.
    Srec,
}

impl Format {
    /// The format named by `--oformat` or `OUTPUT_FORMAT`, if it is a raw
    /// one.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "binary" => Some(Self::Binary),
            "ihex" => Some(Self::Ihex),
            "srec" => Some(Self::Srec),
            _ => None,
        }
    }
}

/// One piece of loadable contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece<'b> {
    /// Its load address.
    pub lma: u64,
    /// Its bytes.
    pub data: &'b [u8],
}

/// Cuts a rendered ELF image into loadable pieces, in section and offset
/// order.
///
/// # Errors
///
/// [`Error::Internal`] when the layout points outside the image.
pub fn pieces<'b>(layout: &Layout<'_>, image: &'b [u8]) -> Result<Vec<Piece<'b>>> {
    let mut out = Vec::new();
    for section in &layout.sections {
        if section.trailer != Trailer::None
            || !section.is_alloc()
            || !section.has_file_bytes()
            || section.size == 0
        {
            continue;
        }
        let mut bounds: Vec<(u64, u64)> = Vec::new();
        for placed in &section.members {
            if placed.size > 0 {
                let _: Member = placed.member;
                bounds.push((placed.offset, placed.size));
            }
        }
        for &(offset, size, _) in &section.fills {
            bounds.push((offset, size));
        }
        for (offset, bytes) in &section.data {
            bounds.push((*offset, u64::try_from(bytes.len()).unwrap_or(0)));
        }
        bounds.sort_unstable();
        let mut cursor = 0u64;
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for (offset, size) in bounds {
            if offset < cursor {
                // Overlapping pieces (a fill inside a member): keep the
                // part past the cursor.
                let end = offset.saturating_add(size);
                if end > cursor {
                    runs.push((cursor, end.wrapping_sub(cursor)));
                    cursor = end;
                }
                continue;
            }
            if offset > cursor {
                runs.push((cursor, offset.wrapping_sub(cursor)));
            }
            runs.push((offset, size));
            cursor = offset.saturating_add(size);
        }
        if cursor < section.size {
            runs.push((cursor, section.size.wrapping_sub(cursor)));
        }
        for (offset, size) in runs {
            if size == 0 {
                continue;
            }
            let start = section
                .offset
                .checked_add(offset)
                .and_then(|s| usize::try_from(s).ok());
            let end = start.and_then(|s| s.checked_add(usize::try_from(size).ok()?));
            let data = start
                .zip(end)
                .and_then(|(s, e)| image.get(s..e))
                .ok_or_else(|| Error::Internal("raw output piece outside the image".into()))?;
            out.push(Piece {
                lma: section.lma.wrapping_add(offset),
                data,
            });
        }
    }
    Ok(out)
}

/// Inserts `piece` into a list sorted by address as BFD does: after the
/// tail when not lower, else before the first entry at or above it.
fn insert_sorted<'b>(list: &mut Vec<Piece<'b>>, piece: Piece<'b>) {
    match list.last() {
        Some(tail) if piece.lma >= tail.lma => list.push(piece),
        None => list.push(piece),
        Some(_) => {
            let at = list.partition_point(|p| p.lma < piece.lma);
            list.insert(at, piece);
        }
    }
}

fn sorted<'b>(pieces: &[Piece<'b>]) -> Vec<Piece<'b>> {
    let mut list = Vec::with_capacity(pieces.len());
    for piece in pieces {
        insert_sorted(&mut list, *piece);
    }
    list
}

/// A flat binary image.
///
/// # Errors
///
/// [`Error::Limit`] when the image would not fit in memory.
pub fn binary(pieces: &[Piece<'_>]) -> Result<Vec<u8>> {
    let Some(low) = pieces.iter().map(|p| p.lma).min() else {
        return Ok(Vec::new());
    };
    let too_large = || Error::Limit("binary image larger than the address space".into());
    let mut size = 0usize;
    for piece in pieces {
        let offset = usize::try_from(piece.lma.wrapping_sub(low)).map_err(|_| too_large())?;
        let end = offset.checked_add(piece.data.len()).ok_or_else(too_large)?;
        size = size.max(end);
    }
    let mut image = vec![0u8; size];
    for piece in pieces {
        let offset = usize::try_from(piece.lma.wrapping_sub(low)).map_err(|_| too_large())?;
        let end = offset.checked_add(piece.data.len()).ok_or_else(too_large)?;
        if let Some(dest) = image.get_mut(offset..end) {
            dest.copy_from_slice(piece.data);
        }
    }
    Ok(image)
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn push_hex(out: &mut Vec<u8>, byte: u8) {
    out.push(HEX[usize::from(byte >> 4)]);
    out.push(HEX[usize::from(byte & 0xf)]);
}

fn ihex_record(out: &mut Vec<u8>, address: u16, kind: u8, data: &[u8]) {
    let count = u8::try_from(data.len()).unwrap_or(u8::MAX);
    let [high, low] = address.to_be_bytes();
    let mut sum = count
        .wrapping_add(high)
        .wrapping_add(low)
        .wrapping_add(kind);
    out.push(b':');
    push_hex(out, count);
    push_hex(out, high);
    push_hex(out, low);
    push_hex(out, kind);
    for &b in data {
        push_hex(out, b);
        sum = sum.wrapping_add(b);
    }
    push_hex(out, sum.wrapping_neg());
    out.extend_from_slice(b"\r\n");
}

/// An Intel HEX image; `start` is the entry point.
///
/// # Errors
///
/// [`Error::Option`] when an address does not fit 32 bits.
pub fn ihex(pieces: &[Piece<'_>], start: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut segbase = 0u64;
    let mut extbase = 0u64;
    for piece in sorted(pieces) {
        let mut where_ = piece.lma;
        if where_ > 0xffff_ffff && where_.wrapping_add(0x8000_0000) > 0xffff_ffff {
            return Err(Error::Option(format!(
                "64-bit address {where_:#x} out of range for Intel Hex file"
            )));
        }
        where_ &= 0xffff_ffff;
        let mut data = piece.data;
        while !data.is_empty() {
            let mut now = data.len().min(16);
            if where_ < extbase
                || where_.wrapping_sub(extbase) < segbase
                || where_.wrapping_sub(extbase).wrapping_sub(segbase) > 0xffff
            {
                if extbase == 0 && where_ <= 0xf_ffff {
                    segbase = where_ & 0xf_0000;
                    let bytes = u16::try_from(segbase >> 4).unwrap_or(0).to_be_bytes();
                    ihex_record(&mut out, 0, 2, &bytes);
                } else {
                    if segbase != 0 {
                        ihex_record(&mut out, 0, 2, &[0, 0]);
                        segbase = 0;
                    }
                    extbase = where_ & 0xffff_0000;
                    if where_ > extbase.wrapping_add(0xffff) {
                        return Err(Error::Option(format!(
                            "address {where_:#x} out of range for Intel Hex file"
                        )));
                    }
                    let bytes = u16::try_from(extbase >> 16).unwrap_or(0).to_be_bytes();
                    ihex_record(&mut out, 0, 4, &bytes);
                }
            }
            let rec_addr = where_.wrapping_sub(extbase.wrapping_add(segbase));
            let now64 = u64::try_from(now).unwrap_or(16);
            if rec_addr.wrapping_add(now64) > 0xffff {
                now = usize::try_from(0x1_0000u64.wrapping_sub(rec_addr)).unwrap_or(1);
            }
            let (chunk, rest) = data.split_at(now.min(data.len()));
            ihex_record(
                &mut out,
                u16::try_from(rec_addr & 0xffff).unwrap_or(0),
                0,
                chunk,
            );
            where_ = where_.wrapping_add(u64::try_from(chunk.len()).unwrap_or(0));
            data = rest;
        }
    }
    if start != 0 {
        let bytes = if start <= 0xf_ffff {
            let segment = u8::try_from((start & 0xf_0000) >> 12).unwrap_or(0);
            let [_, _, high, low] = u32::try_from(start & 0xffff_ffff)
                .unwrap_or(0)
                .to_be_bytes();
            [segment, 0, high, low]
        } else {
            u32::try_from(start & 0xffff_ffff)
                .unwrap_or(0)
                .to_be_bytes()
        };
        let kind = if start <= 0xf_ffff { 3 } else { 5 };
        ihex_record(&mut out, 0, kind, &bytes);
    }
    ihex_record(&mut out, 0, 1, &[]);
    Ok(out)
}

fn srec_record(out: &mut Vec<u8>, kind: u8, address: u64, data: &[u8]) {
    let address_bytes = match kind {
        3 | 7 => 4usize,
        2 | 8 => 3,
        _ => 2,
    };
    let full = address.to_be_bytes();
    let addr = full
        .get(8usize.saturating_sub(address_bytes)..)
        .unwrap_or(&full);
    let length = address_bytes.saturating_add(data.len()).saturating_add(1);
    let length = u8::try_from(length).unwrap_or(u8::MAX);
    let mut sum = length;
    out.push(b'S');
    out.push(b'0'.wrapping_add(kind));
    push_hex(out, length);
    for &b in addr.iter().chain(data) {
        push_hex(out, b);
        sum = sum.wrapping_add(b);
    }
    push_hex(out, 255u8.wrapping_sub(sum));
    out.extend_from_slice(b"\r\n");
}

/// A Motorola S-record image; `name` goes into the `S0` header and `start`
/// into the termination record.
#[must_use]
pub fn srec(pieces: &[Piece<'_>], start: u64, name: &[u8]) -> Vec<u8> {
    let mut kind = 1u8;
    for piece in pieces {
        let last = piece
            .lma
            .wrapping_add(u64::try_from(piece.data.len()).unwrap_or(0))
            .wrapping_sub(1);
        if last <= 0xffff {
        } else if last <= 0xff_ffff && kind <= 2 {
            kind = 2;
        } else {
            kind = 3;
        }
    }
    let mut out = Vec::new();
    let header = name.get(..name.len().min(40)).unwrap_or(name);
    srec_record(&mut out, 0, 0, header);
    for piece in sorted(pieces) {
        for (index, chunk) in piece.data.chunks(16).enumerate() {
            let offset = u64::try_from(index.saturating_mul(16)).unwrap_or(0);
            srec_record(&mut out, kind, piece.lma.wrapping_add(offset), chunk);
        }
    }
    srec_record(&mut out, 10u8.wrapping_sub(kind), start, &[]);
    out
}

/// Renders `format` from a laid out and rendered ELF image.
///
/// # Errors
///
/// As for [`pieces`], [`binary`] and [`ihex`].
pub fn render(
    format: Format,
    layout: &Layout<'_>,
    image: &[u8],
    entry: u64,
    name: &[u8],
) -> Result<Vec<u8>> {
    let pieces = pieces(layout, image)?;
    match format {
        Format::Binary => binary(&pieces),
        Format::Ihex => ihex(&pieces, entry),
        Format::Srec => Ok(srec(&pieces, entry, name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_starts_at_the_lowest_address() {
        let a = [1u8, 2];
        let b = [3u8];
        let image = binary(&[
            Piece {
                lma: 0x1004,
                data: &b,
            },
            Piece {
                lma: 0x1000,
                data: &a,
            },
        ])
        .unwrap();
        assert_eq!(image, [1, 2, 0, 0, 3]);
        assert!(binary(&[]).unwrap().is_empty());
    }

    #[test]
    fn ihex_records() {
        let data: Vec<u8> = (0..20).collect();
        let text = ihex(
            &[Piece {
                lma: 0x10000,
                data: &data,
            }],
            0x10000,
        )
        .unwrap();
        let text = String::from_utf8(text).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], ":020000021000EC");
        assert_eq!(lines[1], ":10000000000102030405060708090A0B0C0D0E0F78");
        assert_eq!(lines[2], ":0400100010111213A6");
        assert_eq!(lines[3], ":0400000310000000E9");
        assert_eq!(lines[4], ":00000001FF");
        let high = ihex(
            &[Piece {
                lma: 0x8000_0000,
                data: &[0xaa],
            }],
            0,
        )
        .unwrap();
        assert!(
            String::from_utf8(high)
                .unwrap()
                .starts_with(":020000048000")
        );
        assert!(
            ihex(
                &[Piece {
                    lma: 1 << 40,
                    data: &[0]
                }],
                0
            )
            .is_err()
        );
    }

    #[test]
    fn srec_records() {
        let text = srec(
            &[Piece {
                lma: 0x100,
                data: b"AB",
            }],
            0x100,
            b"out",
        );
        let text = String::from_utf8(text).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "S00600006F7574A1");
        assert_eq!(lines[1], "S1050100414276");
        assert_eq!(lines[2], "S9030100FB");
        let wide = srec(
            &[Piece {
                lma: 0x1_0000_0000,
                data: b"A",
            }],
            0,
            b"",
        );
        assert!(String::from_utf8(wide).unwrap().contains("S3"));
    }
}
