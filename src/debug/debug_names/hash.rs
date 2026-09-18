//! The hash of `.debug_names`: DJB (`h * 33 + c`, from 5381) over the name
//! case-folded as DWARF 5 section 6.1.1.4.5 asks, which is Unicode simple
//! case folding plus folding U+0130 and U+0131 to `i` (LLVM's
//! `caseFoldingDjbHash`).

/// One case folding rule: tried in order, the first that applies decides.
#[derive(Clone, Copy)]
enum Fold {
    /// Below this code point, nothing changes.
    Below(u32),
    /// This code point folds to that one.
    Is(u32, u32),
    /// Up to this code point, add the offset.
    Add(u32, i32),
    /// Up to this code point, set the low bit.
    Or1(u32),
    /// Up to this code point, when `c % m == r`, add the offset.
    Mod(u32, u32, u32, i32),
}

/// Folds one character (LLVM's `foldCharSimple`).
fn fold_simple(c: u32) -> u32 {
    for rule in FOLD {
        match rule {
            Fold::Below(limit) if c < limit => return c,
            Fold::Is(from, to) if c == from => return to,
            Fold::Add(limit, offset) if c <= limit => return c.wrapping_add_signed(offset),
            Fold::Or1(limit) if c <= limit => return c | 1,
            Fold::Mod(limit, m, r, offset) if c <= limit && c.checked_rem(m) == Some(r) => {
                return c.wrapping_add_signed(offset);
            }
            _ => {}
        }
    }
    c
}

/// Folds one character for `.debug_names` (LLVM's `foldCharDwarf`).
fn fold_dwarf(c: u32) -> u32 {
    if c == 0x130 || c == 0x131 {
        return u32::from(b'i');
    }
    fold_simple(c)
}

/// The `.debug_names` hash of `name`.
#[must_use]
pub(crate) fn hash(name: &[u8]) -> u32 {
    let mut h: u32 = 5381;
    if name.is_ascii() {
        for &c in name {
            h = h
                .wrapping_mul(33)
                .wrapping_add(u32::from(c.to_ascii_lowercase()));
        }
        return h;
    }
    // Invalid UTF-8 becomes U+FFFD per maximal subpart, as LLVM's lenient
    // conversion does.
    let mut buf = [0u8; 4];
    for c in String::from_utf8_lossy(name).chars() {
        let folded = char::from_u32(fold_dwarf(u32::from(c))).unwrap_or(c);
        for &b in folded.encode_utf8(&mut buf).as_bytes() {
            h = h.wrapping_mul(33).wrapping_add(u32::from(b));
        }
    }
    h
}

/// Unicode simple case folding (`CaseFolding.txt` 15.1.0, statuses C and S),
/// as LLVM's `sys::unicode::foldCharSimple`: rules tried in order.
const FOLD: [Fold; 295] = [
    Fold::Below(0x0041),
    Fold::Add(0x005a, 32),
    Fold::Is(0x00b5, 0x03bc),
    Fold::Below(0x00c0),
    Fold::Add(0x00d6, 32),
    Fold::Below(0x00d8),
    Fold::Add(0x00de, 32),
    Fold::Below(0x0100),
    Fold::Or1(0x012e),
    Fold::Below(0x0132),
    Fold::Or1(0x0136),
    Fold::Below(0x0139),
    Fold::Mod(0x0147, 2, 1, 1),
    Fold::Below(0x014a),
    Fold::Or1(0x0176),
    Fold::Is(0x0178, 0x00ff),
    Fold::Below(0x0179),
    Fold::Mod(0x017d, 2, 1, 1),
    Fold::Is(0x017f, 0x0073),
    Fold::Is(0x0181, 0x0253),
    Fold::Below(0x0182),
    Fold::Or1(0x0184),
    Fold::Is(0x0186, 0x0254),
    Fold::Is(0x0187, 0x0188),
    Fold::Below(0x0189),
    Fold::Add(0x018a, 205),
    Fold::Is(0x018b, 0x018c),
    Fold::Is(0x018e, 0x01dd),
    Fold::Is(0x018f, 0x0259),
    Fold::Is(0x0190, 0x025b),
    Fold::Is(0x0191, 0x0192),
    Fold::Is(0x0193, 0x0260),
    Fold::Is(0x0194, 0x0263),
    Fold::Is(0x0196, 0x0269),
    Fold::Is(0x0197, 0x0268),
    Fold::Is(0x0198, 0x0199),
    Fold::Is(0x019c, 0x026f),
    Fold::Is(0x019d, 0x0272),
    Fold::Is(0x019f, 0x0275),
    Fold::Below(0x01a0),
    Fold::Or1(0x01a4),
    Fold::Is(0x01a6, 0x0280),
    Fold::Is(0x01a7, 0x01a8),
    Fold::Is(0x01a9, 0x0283),
    Fold::Is(0x01ac, 0x01ad),
    Fold::Is(0x01ae, 0x0288),
    Fold::Is(0x01af, 0x01b0),
    Fold::Below(0x01b1),
    Fold::Add(0x01b2, 217),
    Fold::Below(0x01b3),
    Fold::Mod(0x01b5, 2, 1, 1),
    Fold::Is(0x01b7, 0x0292),
    Fold::Below(0x01b8),
    Fold::Mod(0x01bc, 4, 0, 1),
    Fold::Is(0x01c4, 0x01c6),
    Fold::Is(0x01c5, 0x01c6),
    Fold::Is(0x01c7, 0x01c9),
    Fold::Is(0x01c8, 0x01c9),
    Fold::Is(0x01ca, 0x01cc),
    Fold::Below(0x01cb),
    Fold::Mod(0x01db, 2, 1, 1),
    Fold::Below(0x01de),
    Fold::Or1(0x01ee),
    Fold::Is(0x01f1, 0x01f3),
    Fold::Below(0x01f2),
    Fold::Or1(0x01f4),
    Fold::Is(0x01f6, 0x0195),
    Fold::Is(0x01f7, 0x01bf),
    Fold::Below(0x01f8),
    Fold::Or1(0x021e),
    Fold::Is(0x0220, 0x019e),
    Fold::Below(0x0222),
    Fold::Or1(0x0232),
    Fold::Is(0x023a, 0x2c65),
    Fold::Is(0x023b, 0x023c),
    Fold::Is(0x023d, 0x019a),
    Fold::Is(0x023e, 0x2c66),
    Fold::Is(0x0241, 0x0242),
    Fold::Is(0x0243, 0x0180),
    Fold::Is(0x0244, 0x0289),
    Fold::Is(0x0245, 0x028c),
    Fold::Below(0x0246),
    Fold::Or1(0x024e),
    Fold::Is(0x0345, 0x03b9),
    Fold::Below(0x0370),
    Fold::Or1(0x0372),
    Fold::Is(0x0376, 0x0377),
    Fold::Is(0x037f, 0x03f3),
    Fold::Is(0x0386, 0x03ac),
    Fold::Below(0x0388),
    Fold::Add(0x038a, 37),
    Fold::Is(0x038c, 0x03cc),
    Fold::Below(0x038e),
    Fold::Add(0x038f, 63),
    Fold::Below(0x0391),
    Fold::Add(0x03a1, 32),
    Fold::Below(0x03a3),
    Fold::Add(0x03ab, 32),
    Fold::Is(0x03c2, 0x03c3),
    Fold::Is(0x03cf, 0x03d7),
    Fold::Is(0x03d0, 0x03b2),
    Fold::Is(0x03d1, 0x03b8),
    Fold::Is(0x03d5, 0x03c6),
    Fold::Is(0x03d6, 0x03c0),
    Fold::Below(0x03d8),
    Fold::Or1(0x03ee),
    Fold::Is(0x03f0, 0x03ba),
    Fold::Is(0x03f1, 0x03c1),
    Fold::Is(0x03f4, 0x03b8),
    Fold::Is(0x03f5, 0x03b5),
    Fold::Is(0x03f7, 0x03f8),
    Fold::Is(0x03f9, 0x03f2),
    Fold::Is(0x03fa, 0x03fb),
    Fold::Below(0x03fd),
    Fold::Add(0x03ff, -130),
    Fold::Below(0x0400),
    Fold::Add(0x040f, 80),
    Fold::Below(0x0410),
    Fold::Add(0x042f, 32),
    Fold::Below(0x0460),
    Fold::Or1(0x0480),
    Fold::Below(0x048a),
    Fold::Or1(0x04be),
    Fold::Is(0x04c0, 0x04cf),
    Fold::Below(0x04c1),
    Fold::Mod(0x04cd, 2, 1, 1),
    Fold::Below(0x04d0),
    Fold::Or1(0x052e),
    Fold::Below(0x0531),
    Fold::Add(0x0556, 48),
    Fold::Below(0x10a0),
    Fold::Add(0x10c5, 7264),
    Fold::Below(0x10c7),
    Fold::Mod(0x10cd, 6, 5, 7264),
    Fold::Below(0x13f8),
    Fold::Add(0x13fd, -8),
    Fold::Is(0x1c80, 0x0432),
    Fold::Is(0x1c81, 0x0434),
    Fold::Is(0x1c82, 0x043e),
    Fold::Below(0x1c83),
    Fold::Add(0x1c84, -6210),
    Fold::Is(0x1c85, 0x0442),
    Fold::Is(0x1c86, 0x044a),
    Fold::Is(0x1c87, 0x0463),
    Fold::Is(0x1c88, 0xa64b),
    Fold::Below(0x1c90),
    Fold::Add(0x1cba, -3008),
    Fold::Below(0x1cbd),
    Fold::Add(0x1cbf, -3008),
    Fold::Below(0x1e00),
    Fold::Or1(0x1e94),
    Fold::Is(0x1e9b, 0x1e61),
    Fold::Is(0x1e9e, 0x00df),
    Fold::Below(0x1ea0),
    Fold::Or1(0x1efe),
    Fold::Below(0x1f08),
    Fold::Add(0x1f0f, -8),
    Fold::Below(0x1f18),
    Fold::Add(0x1f1d, -8),
    Fold::Below(0x1f28),
    Fold::Add(0x1f2f, -8),
    Fold::Below(0x1f38),
    Fold::Add(0x1f3f, -8),
    Fold::Below(0x1f48),
    Fold::Add(0x1f4d, -8),
    Fold::Below(0x1f59),
    Fold::Mod(0x1f5f, 2, 1, -8),
    Fold::Below(0x1f68),
    Fold::Add(0x1f6f, -8),
    Fold::Below(0x1f88),
    Fold::Add(0x1f8f, -8),
    Fold::Below(0x1f98),
    Fold::Add(0x1f9f, -8),
    Fold::Below(0x1fa8),
    Fold::Add(0x1faf, -8),
    Fold::Below(0x1fb8),
    Fold::Add(0x1fb9, -8),
    Fold::Below(0x1fba),
    Fold::Add(0x1fbb, -74),
    Fold::Is(0x1fbc, 0x1fb3),
    Fold::Is(0x1fbe, 0x03b9),
    Fold::Below(0x1fc8),
    Fold::Add(0x1fcb, -86),
    Fold::Is(0x1fcc, 0x1fc3),
    Fold::Is(0x1fd3, 0x0390),
    Fold::Below(0x1fd8),
    Fold::Add(0x1fd9, -8),
    Fold::Below(0x1fda),
    Fold::Add(0x1fdb, -100),
    Fold::Is(0x1fe3, 0x03b0),
    Fold::Below(0x1fe8),
    Fold::Add(0x1fe9, -8),
    Fold::Below(0x1fea),
    Fold::Add(0x1feb, -112),
    Fold::Is(0x1fec, 0x1fe5),
    Fold::Below(0x1ff8),
    Fold::Add(0x1ff9, -128),
    Fold::Below(0x1ffa),
    Fold::Add(0x1ffb, -126),
    Fold::Is(0x1ffc, 0x1ff3),
    Fold::Is(0x2126, 0x03c9),
    Fold::Is(0x212a, 0x006b),
    Fold::Is(0x212b, 0x00e5),
    Fold::Is(0x2132, 0x214e),
    Fold::Below(0x2160),
    Fold::Add(0x216f, 16),
    Fold::Is(0x2183, 0x2184),
    Fold::Below(0x24b6),
    Fold::Add(0x24cf, 26),
    Fold::Below(0x2c00),
    Fold::Add(0x2c2f, 48),
    Fold::Is(0x2c60, 0x2c61),
    Fold::Is(0x2c62, 0x026b),
    Fold::Is(0x2c63, 0x1d7d),
    Fold::Is(0x2c64, 0x027d),
    Fold::Below(0x2c67),
    Fold::Mod(0x2c6b, 2, 1, 1),
    Fold::Is(0x2c6d, 0x0251),
    Fold::Is(0x2c6e, 0x0271),
    Fold::Is(0x2c6f, 0x0250),
    Fold::Is(0x2c70, 0x0252),
    Fold::Below(0x2c72),
    Fold::Mod(0x2c75, 3, 2, 1),
    Fold::Below(0x2c7e),
    Fold::Add(0x2c7f, -10815),
    Fold::Below(0x2c80),
    Fold::Or1(0x2ce2),
    Fold::Below(0x2ceb),
    Fold::Mod(0x2ced, 2, 1, 1),
    Fold::Below(0x2cf2),
    Fold::Mod(0xa640, 31054, 11506, 1),
    Fold::Below(0xa642),
    Fold::Or1(0xa66c),
    Fold::Below(0xa680),
    Fold::Or1(0xa69a),
    Fold::Below(0xa722),
    Fold::Or1(0xa72e),
    Fold::Below(0xa732),
    Fold::Or1(0xa76e),
    Fold::Below(0xa779),
    Fold::Mod(0xa77b, 2, 1, 1),
    Fold::Is(0xa77d, 0x1d79),
    Fold::Below(0xa77e),
    Fold::Or1(0xa786),
    Fold::Is(0xa78b, 0xa78c),
    Fold::Is(0xa78d, 0x0265),
    Fold::Below(0xa790),
    Fold::Or1(0xa792),
    Fold::Below(0xa796),
    Fold::Or1(0xa7a8),
    Fold::Is(0xa7aa, 0x0266),
    Fold::Is(0xa7ab, 0x025c),
    Fold::Is(0xa7ac, 0x0261),
    Fold::Is(0xa7ad, 0x026c),
    Fold::Is(0xa7ae, 0x026a),
    Fold::Is(0xa7b0, 0x029e),
    Fold::Is(0xa7b1, 0x0287),
    Fold::Is(0xa7b2, 0x029d),
    Fold::Is(0xa7b3, 0xab53),
    Fold::Below(0xa7b4),
    Fold::Or1(0xa7c2),
    Fold::Is(0xa7c4, 0xa794),
    Fold::Is(0xa7c5, 0x0282),
    Fold::Is(0xa7c6, 0x1d8e),
    Fold::Below(0xa7c7),
    Fold::Mod(0xa7c9, 2, 1, 1),
    Fold::Below(0xa7d0),
    Fold::Mod(0xa7d6, 6, 0, 1),
    Fold::Below(0xa7d8),
    Fold::Mod(0xa7f5, 29, 19, 1),
    Fold::Below(0xab70),
    Fold::Add(0xabbf, -38864),
    Fold::Is(0xfb05, 0xfb06),
    Fold::Below(0xff21),
    Fold::Add(0xff3a, 32),
    Fold::Below(0x10400),
    Fold::Add(0x10427, 40),
    Fold::Below(0x104b0),
    Fold::Add(0x104d3, 40),
    Fold::Below(0x10570),
    Fold::Add(0x1057a, 39),
    Fold::Below(0x1057c),
    Fold::Add(0x1058a, 39),
    Fold::Below(0x1058c),
    Fold::Add(0x10592, 39),
    Fold::Below(0x10594),
    Fold::Add(0x10595, 39),
    Fold::Below(0x10c80),
    Fold::Add(0x10cb2, 64),
    Fold::Below(0x118a0),
    Fold::Add(0x118bf, 32),
    Fold::Below(0x16e40),
    Fold::Add(0x16e5f, 32),
    Fold::Below(0x1e900),
    Fold::Add(0x1e921, 34),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_names_fold_case() {
        assert_eq!(hash(b""), 5381);
        assert_eq!(hash(b"main"), hash(b"MAIN"));
        // djb("a") = 5381 * 33 + 97.
        assert_eq!(hash(b"a"), 5381 * 33 + 97);
    }

    #[test]
    fn unicode_names_fold_case() {
        assert_eq!(hash("ÉTÉ".as_bytes()), hash("été".as_bytes()));
        assert_eq!(hash("ΣΑΣ".as_bytes()), hash("σασ".as_bytes()));
        assert_eq!(hash("İ".as_bytes()), hash(b"i"));
        assert_eq!(fold_simple(0xb5), 0x3bc);
        assert_eq!(fold_simple(0x1e9e), 0xdf);
        assert_eq!(fold_simple(0x10400), 0x10428);
    }
}
