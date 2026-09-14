//! The sequences section of a compressed block (RFC 8878 section
//! 3.1.1.3.2), decoded and executed in one pass.

use std::sync::OnceLock;

use super::bits::BackwardBits;
use super::fse;
use crate::debug::compress::copy::copy_match;

const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
const LL_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
const ML_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// Predefined distributions (RFC 8878 section 3.1.1.3.2.2).
const LL_DEFAULT: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const ML_DEFAULT: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
const OF_DEFAULT: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

#[derive(Clone, Copy)]
enum Kind {
    LiteralLength,
    Offset,
    MatchLength,
}

impl Kind {
    const fn max_symbol(self) -> usize {
        match self {
            Self::LiteralLength => 35,
            Self::Offset => 31,
            Self::MatchLength => 52,
        }
    }

    const fn max_log(self) -> u32 {
        match self {
            Self::LiteralLength | Self::MatchLength => 9,
            Self::Offset => 8,
        }
    }

    fn predefined(self) -> &'static fse::Table {
        static TABLES: OnceLock<[Box<fse::Table>; 3]> = OnceLock::new();
        let tables = TABLES.get_or_init(|| {
            let make = |norm: &[i16], log| {
                let mut table = Box::new(fse::Table::new());
                // The predefined distributions are valid by construction.
                let _ = table.build(norm, log);
                table
            };
            [
                make(&LL_DEFAULT, 6),
                make(&OF_DEFAULT, 5),
                make(&ML_DEFAULT, 6),
            ]
        });
        match self {
            Self::LiteralLength => &tables[0],
            Self::Offset => &tables[1],
            Self::MatchLength => &tables[2],
        }
    }
}

/// Decoding state that persists between the blocks of a frame.
pub(super) struct State {
    tables: [Box<fse::Table>; 3],
    /// Whether each table has been set (for `Repeat_Mode`).
    valid: [bool; 3],
    repeat: [usize; 3],
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            tables: [
                Box::new(fse::Table::new()),
                Box::new(fse::Table::new()),
                Box::new(fse::Table::new()),
            ],
            valid: [false; 3],
            repeat: [1, 4, 8],
        }
    }

    /// Reads one table's mode and description. Returns the bytes used.
    fn update_table(
        &mut self,
        index: usize,
        kind: Kind,
        mode: u8,
        data: &[u8],
    ) -> Result<usize, &'static str> {
        const BAD: &str = "zstd sequence table";
        let table = self.tables.get_mut(index).ok_or(BAD)?;
        let used = match mode {
            0 => {
                **table = kind.predefined().clone();
                0
            }
            1 => {
                let &symbol = data.first().ok_or(BAD)?;
                if usize::from(symbol) > kind.max_symbol() {
                    return Err(BAD);
                }
                table.rle(symbol);
                1
            }
            2 => {
                let mut norm = [0i16; 256];
                let (used, log, symbols) =
                    fse::read_ncount(data, kind.max_symbol(), kind.max_log(), &mut norm)?;
                table.build(norm.get(..symbols).ok_or(BAD)?, log)?;
                used
            }
            _ => {
                if !self.valid.get(index).copied().unwrap_or(false) {
                    return Err("zstd sequence table (repeat without a previous table)");
                }
                0
            }
        };
        if let Some(valid) = self.valid.get_mut(index) {
            *valid = true;
        }
        Ok(used)
    }
}

/// Decodes the sequences section `data` and executes it: literals come
/// from `literals`, output goes to `out` at `o`, and matches may reach back
/// to `frame_start`. Returns the new output position.
pub(super) fn execute(
    state: &mut State,
    data: &[u8],
    literals: &[u8],
    out: &mut [u8],
    mut o: usize,
    frame_start: usize,
) -> Result<usize, &'static str> {
    const BAD: &str = "zstd sequences section";
    const TOO_LARGE: &str = "zstd data (larger than the declared size)";
    let (&b0, rest) = data.split_first().ok_or(BAD)?;
    let (count, mut rest) = match b0 {
        0 => {
            if !rest.is_empty() {
                return Err(BAD);
            }
            return copy_literals(literals, out, o).ok_or(TOO_LARGE);
        }
        1..=127 => (usize::from(b0), rest),
        128..=254 => {
            let (&b1, rest) = rest.split_first().ok_or(BAD)?;
            ((usize::from(b0 & 0x7f) << 8) | usize::from(b1), rest)
        }
        255 => {
            let (b12, rest) = rest.split_at_checked(2).ok_or(BAD)?;
            let n = usize::from(u16::from_le_bytes([b12[0], b12[1]]));
            (n.wrapping_add(0x7f00), rest)
        }
    };
    let (&modes, tail) = rest.split_first().ok_or(BAD)?;
    rest = tail;
    if modes & 3 != 0 {
        return Err(BAD);
    }
    for (index, kind, shift) in [
        (0, Kind::LiteralLength, 6),
        (1, Kind::Offset, 4),
        (2, Kind::MatchLength, 2),
    ] {
        let used = state.update_table(index, kind, (modes >> shift) & 3, rest)?;
        rest = rest.get(used..).ok_or(BAD)?;
    }

    let [ll_table, of_table, ml_table] = &state.tables;
    let mut bits = BackwardBits::new(rest)?;
    let mut ll_state = bits.read(ll_table.log);
    let mut of_state = bits.read(of_table.log);
    let mut ml_state = bits.read(ml_table.log);
    let mut repeat = state.repeat;
    let mut lit = 0usize;

    for i in 0..count {
        let ll_entry = ll_table.get(ll_state);
        let of_entry = of_table.get(of_state);
        let ml_entry = ml_table.get(ml_state);

        let of_code = u32::from(of_entry.symbol);
        if of_code > 31 {
            return Err(BAD);
        }
        let offset_value = (1u64 << of_code).wrapping_add(bits.read(of_code)) as usize;
        let ml_code = usize::from(ml_entry.symbol);
        let match_len = (*ML_BASE.get(ml_code).ok_or(BAD)? as usize)
            .wrapping_add(bits.read(u32::from(*ML_BITS.get(ml_code).ok_or(BAD)?)) as usize);
        let ll_code = usize::from(ll_entry.symbol);
        let lit_len = (*LL_BASE.get(ll_code).ok_or(BAD)? as usize)
            .wrapping_add(bits.read(u32::from(*LL_BITS.get(ll_code).ok_or(BAD)?)) as usize);

        let offset = if offset_value > 3 {
            let offset = offset_value.wrapping_sub(3);
            repeat = [offset, repeat[0], repeat[1]];
            offset
        } else {
            let index = offset_value.wrapping_sub(usize::from(lit_len != 0));
            match index {
                0 => repeat[0],
                1 => {
                    repeat = [repeat[1], repeat[0], repeat[2]];
                    repeat[0]
                }
                2 => {
                    repeat = [repeat[2], repeat[0], repeat[1]];
                    repeat[0]
                }
                _ => {
                    let offset = repeat[0].wrapping_sub(1);
                    if offset == 0 {
                        return Err("zstd offset (zero)");
                    }
                    repeat = [offset, repeat[0], repeat[1]];
                    offset
                }
            }
        };

        if i.wrapping_add(1) < count {
            ll_state = u64::from(ll_entry.base).wrapping_add(bits.read(u32::from(ll_entry.bits)));
            ml_state = u64::from(ml_entry.base).wrapping_add(bits.read(u32::from(ml_entry.bits)));
            of_state = u64::from(of_entry.base).wrapping_add(bits.read(u32::from(of_entry.bits)));
        }

        let lit_end = lit.checked_add(lit_len).ok_or(BAD)?;
        let src = literals
            .get(lit..lit_end)
            .ok_or("zstd sequence (not enough literals)")?;
        let o_end = o.checked_add(lit_len).ok_or(TOO_LARGE)?;
        out.get_mut(o..o_end).ok_or(TOO_LARGE)?.copy_from_slice(src);
        lit = lit_end;
        o = o_end;

        if offset > o.wrapping_sub(frame_start) {
            return Err("zstd offset (before the start of the frame)");
        }
        o = copy_match(out, o, offset, match_len).ok_or(TOO_LARGE)?;
    }
    if !bits.finished() {
        return Err("zstd sequence bitstream (bad length)");
    }
    state.repeat = repeat;
    copy_literals(literals.get(lit..).unwrap_or_default(), out, o).ok_or(TOO_LARGE)
}

fn copy_literals(literals: &[u8], out: &mut [u8], o: usize) -> Option<usize> {
    let end = o.checked_add(literals.len())?;
    out.get_mut(o..end)?.copy_from_slice(literals);
    Some(end)
}
