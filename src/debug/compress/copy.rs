//! LZ77 match copying shared by the DEFLATE and Zstandard decoders.

/// Copies a `length`-byte match from `distance` bytes back, starting at
/// output position `o`. Returns the new output position, or `None` if the
/// match starts before the output (or `distance` is 0) or ends past it.
///
/// Short matches whose source is at least eight bytes back are copied a
/// word at a time, overshooting by up to seven bytes; later data always
/// overwrites the overshoot, because output is produced in order.
#[inline(always)]
pub(super) fn copy_match(
    out: &mut [u8],
    o: usize,
    distance: usize,
    length: usize,
) -> Option<usize> {
    let src = o.checked_sub(distance)?;
    let end = o.checked_add(length)?;
    if distance == 0 || end > out.len() {
        return None;
    }
    if distance >= 8 && length <= 64 && end.wrapping_add(8) <= out.len() {
        let (mut s, mut d) = (src, o);
        loop {
            let word: [u8; 8] = out.get(s..s.wrapping_add(8))?.try_into().ok()?;
            out.get_mut(d..d.wrapping_add(8))?.copy_from_slice(&word);
            s = s.wrapping_add(8);
            d = d.wrapping_add(8);
            if d >= end {
                break;
            }
        }
    } else if distance >= length {
        out.copy_within(src..src.wrapping_add(length), o);
    } else if distance == 1 {
        let byte = *out.get(src)?;
        out.get_mut(o..end)?.fill(byte);
    } else {
        // Overlapping: the output repeats with period `distance`. Copy
        // whole periods, doubling the copied run each time.
        let mut done = 0usize;
        while done < length {
            let n = length.wrapping_sub(done).min(distance.wrapping_add(done));
            out.copy_within(src..src.wrapping_add(n), o.wrapping_add(done));
            done = done.wrapping_add(n);
        }
    }
    Some(end)
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::copy_match;

    fn reference(out: &mut [u8], o: usize, distance: usize, length: usize) {
        for i in 0..length {
            out[o + i] = out[o + i - distance];
        }
    }

    #[test]
    fn matches_bytewise_copy() {
        for distance in 1..40 {
            for length in [1, 2, 3, 7, 8, 9, 15, 16, 17, 63, 64, 65, 200, 1000] {
                for slack in [0, 3, 8, 20] {
                    let o = 50;
                    let size = o + length + slack;
                    let mut a: Vec<u8> = (0..size).map(|i| (i * 7 + 3) as u8).collect();
                    let mut b = a.clone();
                    reference(&mut a, o, distance, length);
                    assert_eq!(copy_match(&mut b, o, distance, length), Some(o + length));
                    assert_eq!(&a[..o + length], &b[..o + length], "{distance} {length}");
                }
            }
        }
    }

    #[test]
    fn rejects_out_of_range() {
        let mut out = vec![0u8; 100];
        assert_eq!(copy_match(&mut out, 10, 11, 3), None);
        assert_eq!(copy_match(&mut out, 10, 0, 3), None);
        assert_eq!(copy_match(&mut out, 98, 5, 3), None);
        assert_eq!(copy_match(&mut out, 97, 5, 3), Some(100));
    }
}
