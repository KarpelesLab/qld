//! Which target an ELF link is for, when `-m` does not say.
//!
//! GNU ld takes the emulation from `-m`, or else uses the one it was
//! configured with. qld takes it from `-m`, or else from the first input
//! that names one — an ELF object or shared library, a GCC LTO object (an
//! ELF file with a real `e_machine`), or LLVM bitcode (the triple in its
//! symbol table) — and only then falls back to the host, as a GNU ld built
//! for the host would.

use crate::target::{Architecture, Endianness, PointerWidth, Target};

/// The target when neither `-m` nor any input names one: the host's, when
/// qld links ELF for it, else x86-64 Linux.
#[must_use]
pub fn default_target() -> Target {
    let arch = if cfg!(target_arch = "aarch64") {
        Architecture::Aarch64
    } else if cfg!(target_arch = "riscv64") {
        Architecture::Riscv64
    } else if cfg!(target_arch = "loongarch64") {
        Architecture::LoongArch64
    } else if cfg!(all(target_arch = "powerpc64", target_endian = "little")) {
        Architecture::PowerPc64
    } else if cfg!(target_arch = "x86") {
        Architecture::X86
    } else {
        return Target::X86_64_LINUX;
    };
    target_for(arch, Endianness::Little)
}

/// A Linux ELF target for `arch` in byte order `endian`.
fn target_for(arch: Architecture, endian: Endianness) -> Target {
    let mut target = Target::X86_64_LINUX;
    target.arch = arch;
    target.endian = endian;
    target.pointer_width = match arch {
        Architecture::X86_64X32 | Architecture::X86 | Architecture::Arm | Architecture::Riscv32 => {
            PointerWidth::Bits32
        }
        _ => PointerWidth::Bits64,
    };
    target
}

/// The target an LLVM target triple (`aarch64-unknown-linux-gnu`) names,
/// when it is an architecture qld knows.
#[must_use]
pub fn from_triple(triple: &str) -> Option<Target> {
    let mut parts = triple.split('-');
    let cpu = parts.next()?;
    let x32 = parts.any(|part| part.ends_with("x32"));
    let (arch, endian) = match cpu {
        "x86_64" | "amd64" if x32 => (Architecture::X86_64X32, Endianness::Little),
        "x86_64" | "amd64" => (Architecture::X86_64, Endianness::Little),
        "i386" | "i486" | "i586" | "i686" => (Architecture::X86, Endianness::Little),
        "aarch64" | "arm64" => (Architecture::Aarch64, Endianness::Little),
        "aarch64_be" => (Architecture::Aarch64, Endianness::Big),
        "riscv64" => (Architecture::Riscv64, Endianness::Little),
        "riscv32" => (Architecture::Riscv32, Endianness::Little),
        "powerpc64le" | "ppc64le" => (Architecture::PowerPc64, Endianness::Little),
        "powerpc64" | "ppc64" => (Architecture::PowerPc64, Endianness::Big),
        "loongarch64" => (Architecture::LoongArch64, Endianness::Little),
        "s390x" | "systemz" => (Architecture::S390x, Endianness::Big),
        "armeb" | "thumbeb" => (Architecture::Arm, Endianness::Big),
        cpu if cpu.starts_with("arm") || cpu.starts_with("thumb") => {
            (Architecture::Arm, Endianness::Little)
        }
        _ => return None,
    };
    Some(target_for(arch, endian))
}

/// The target LLVM bitcode `data` was compiled for, from the triple its
/// symbol table records. `None` when there is none (bitcode older than
/// LLVM 5 has no symbol table) or the data is malformed; never panics.
#[must_use]
pub fn of_bitcode(data: &[u8]) -> Option<Target> {
    from_triple(core::str::from_utf8(bitcode_triple(data)?).ok()?)
}

/// `BC\xc0\xde`.
const BITCODE_MAGIC: [u8; 4] = [0x42, 0x43, 0xc0, 0xde];
/// The bitcode wrapper header's magic, `0x0b17c0de`.
const WRAPPER_MAGIC: u32 = 0x0b17_c0de;
/// Top-level block ids of the blobs that hold the target triple.
const STRTAB_BLOCK_ID: u64 = 23;
const SYMTAB_BLOCK_ID: u64 = 25;
/// The byte offset of `TargetTriple` (a `{offset, size}` pair into the
/// string table) in LLVM's `irsymtab::storage::Header`: after `Version`,
/// `Producer` and four ranges.
const TRIPLE_FIELD: usize = 44;

/// The target triple recorded in bitcode `data`'s symbol table.
fn bitcode_triple(data: &[u8]) -> Option<&[u8]> {
    let data = unwrap_bitcode(data)?;
    let mut reader = BitReader {
        data,
        bit: BITCODE_MAGIC.len() * 8,
    };
    let (mut symtab, mut strtab) = (None, None);
    // Top-level blocks, abbreviation width 2. Anything that is not a block
    // (the zero padding of a wrapped module) ends the walk.
    while symtab.is_none() || strtab.is_none() {
        if reader.remaining() < 32 || reader.fixed(2)? != ENTER_SUBBLOCK {
            break;
        }
        let (id, width, end) = reader.enter_block()?;
        match id {
            SYMTAB_BLOCK_ID if symtab.is_none() => symtab = reader.blob_in_block(width, end)?,
            STRTAB_BLOCK_ID if strtab.is_none() => strtab = reader.blob_in_block(width, end)?,
            _ => {}
        }
        reader.bit = end;
    }
    let (symtab, strtab) = (symtab?, strtab?);
    let word = |at: usize| -> Option<usize> {
        let bytes = symtab.get(at..at.checked_add(4)?)?;
        usize::try_from(u32::from_le_bytes(bytes.try_into().ok()?)).ok()
    };
    let offset = word(TRIPLE_FIELD)?;
    let size = word(TRIPLE_FIELD + 4)?;
    strtab.get(offset..offset.checked_add(size)?)
}

/// The bitcode inside `data`, which may be raw or in a wrapper header.
fn unwrap_bitcode(data: &[u8]) -> Option<&[u8]> {
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
    };
    let bitcode = if word(0)? == WRAPPER_MAGIC {
        let offset = usize::try_from(word(8)?).ok()?;
        let size = usize::try_from(word(12)?).ok()?;
        data.get(offset..offset.checked_add(size)?)?
    } else {
        data
    };
    bitcode.starts_with(&BITCODE_MAGIC).then_some(bitcode)
}

/// Abbreviation ids every block has.
const END_BLOCK: u64 = 0;
const ENTER_SUBBLOCK: u64 = 1;
const DEFINE_ABBREV: u64 = 2;
const UNABBREV_RECORD: u64 = 3;
/// The record code of `SYMTAB_BLOB` and `STRTAB_BLOB`.
const BLOB_RECORD: u64 = 1;
/// Bounds that keep hostile input from making the walk expensive.
const MAX_ABBREVS: usize = 64;
const MAX_OPERANDS: u64 = 64;

/// One operand of an abbreviation.
#[derive(Clone, Copy)]
enum Op {
    Literal(u64),
    Fixed(u32),
    Vbr(u32),
    Array,
    Char6,
    Blob,
}

/// An LLVM bitstream reader: bits are read least significant first.
struct BitReader<'a> {
    data: &'a [u8],
    /// The position, in bits.
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn remaining(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.bit)
    }

    fn fixed(&mut self, width: u32) -> Option<u64> {
        if width > 64 || self.remaining() < width as usize {
            return None;
        }
        let mut value = 0u64;
        for i in 0..width {
            let byte = self.data.get(self.bit / 8)?;
            value |= u64::from((byte >> (self.bit % 8)) & 1) << i;
            self.bit += 1;
        }
        Some(value)
    }

    fn vbr(&mut self, width: u32) -> Option<u64> {
        if !(2..=32).contains(&width) {
            return None;
        }
        let high = 1u64 << (width - 1);
        let (mut value, mut shift) = (0u64, 0u32);
        loop {
            let chunk = self.fixed(width)?;
            if shift >= 64 {
                return None;
            }
            value |= (chunk & (high - 1)).checked_shl(shift)?;
            if chunk & high == 0 {
                return Some(value);
            }
            shift += width - 1;
        }
    }

    fn align32(&mut self) -> Option<()> {
        self.bit = self.bit.checked_add(31)? & !31;
        (self.bit <= self.data.len() * 8).then_some(())
    }

    /// Reads a block header after its `ENTER_SUBBLOCK`: the block id, its
    /// abbreviation width and the bit position of its end.
    fn enter_block(&mut self) -> Option<(u64, u32, usize)> {
        let id = self.vbr(8)?;
        let width = u32::try_from(self.vbr(4)?).ok()?;
        self.align32()?;
        let words = usize::try_from(self.fixed(32)?).ok()?;
        let end = words.checked_mul(32)?.checked_add(self.bit)?;
        if !(1..=32).contains(&width) || end > self.data.len() * 8 {
            return None;
        }
        Some((id, width, end))
    }

    /// The first `BLOB_RECORD` blob of the block being read, whose
    /// abbreviations are `width` bits wide and which ends at bit `end`.
    fn blob_in_block(&mut self, width: u32, end: usize) -> Option<Option<&'a [u8]>> {
        let mut abbrevs: Vec<Vec<Op>> = Vec::new();
        while self.bit < end {
            match self.fixed(width)? {
                END_BLOCK => return Some(None),
                ENTER_SUBBLOCK => {
                    let (_, _, sub_end) = self.enter_block()?;
                    self.bit = sub_end;
                }
                DEFINE_ABBREV => {
                    if abbrevs.len() >= MAX_ABBREVS {
                        return None;
                    }
                    abbrevs.push(self.define_abbrev()?);
                }
                UNABBREV_RECORD => {
                    let _code = self.vbr(6)?;
                    let count = self.vbr(6)?;
                    if count > MAX_OPERANDS {
                        return None;
                    }
                    for _ in 0..count {
                        self.vbr(6)?;
                    }
                }
                id => {
                    let index = usize::try_from(id - 4).ok()?;
                    let ops = abbrevs.get(index)?.clone();
                    if let Some(blob) = self.abbreviated_record(&ops)? {
                        return Some(Some(blob));
                    }
                }
            }
        }
        Some(None)
    }

    fn define_abbrev(&mut self) -> Option<Vec<Op>> {
        let count = self.vbr(5)?;
        if count > MAX_OPERANDS {
            return None;
        }
        let mut ops = Vec::new();
        for _ in 0..count {
            let op = if self.fixed(1)? == 1 {
                Op::Literal(self.vbr(8)?)
            } else {
                match self.fixed(3)? {
                    1 => Op::Fixed(u32::try_from(self.vbr(5)?).ok()?),
                    2 => Op::Vbr(u32::try_from(self.vbr(5)?).ok()?),
                    3 => Op::Array,
                    4 => Op::Char6,
                    5 => Op::Blob,
                    _ => return None,
                }
            };
            ops.push(op);
        }
        Some(ops)
    }

    /// Reads a record with abbreviation `ops`: `Some(Some(blob))` when it
    /// is a `BLOB_RECORD` with a blob operand.
    fn abbreviated_record(&mut self, ops: &[Op]) -> Option<Option<&'a [u8]>> {
        let mut code = None;
        let mut blob = None;
        let mut i = 0;
        while let Some(&op) = ops.get(i) {
            i += 1;
            let value = match op {
                Op::Literal(value) => Some(value),
                Op::Fixed(width) => Some(self.fixed(width)?),
                Op::Vbr(width) => Some(if width == 0 { 0 } else { self.vbr(width)? }),
                Op::Char6 => Some(self.fixed(6)?),
                Op::Array => {
                    // The element operand follows; the array ends the record.
                    let element = *ops.get(i)?;
                    let count = self.vbr(6)?;
                    let bits = match element {
                        Op::Fixed(width) | Op::Vbr(width) => width,
                        Op::Char6 => 6,
                        Op::Literal(_) => 0,
                        Op::Array | Op::Blob => return None,
                    };
                    if bits > 0 {
                        // Each element takes at least `bits` bits.
                        if count > (self.remaining() / bits as usize) as u64 {
                            return None;
                        }
                        for _ in 0..count {
                            match element {
                                Op::Vbr(width) => self.vbr(width)?,
                                _ => self.fixed(bits)?,
                            };
                        }
                    }
                    break;
                }
                Op::Blob => {
                    let len = usize::try_from(self.vbr(6)?).ok()?;
                    self.align32()?;
                    let start = self.bit / 8;
                    let bytes = self.data.get(start..start.checked_add(len)?)?;
                    self.bit = self.bit.checked_add(len.checked_mul(8)?)?;
                    self.align32()?;
                    blob = Some(bytes);
                    None
                }
            };
            if code.is_none() {
                code = value;
            }
        }
        Some(blob.filter(|_| code == Some(BLOB_RECORD)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triples() {
        let arch = |triple: &str| from_triple(triple).map(|t| (t.arch, t.endian, t.pointer_width));
        assert_eq!(
            arch("aarch64-unknown-linux-gnu"),
            Some((
                Architecture::Aarch64,
                Endianness::Little,
                PointerWidth::Bits64
            ))
        );
        assert_eq!(
            arch("i686-pc-linux-gnu"),
            Some((Architecture::X86, Endianness::Little, PointerWidth::Bits32))
        );
        assert_eq!(
            arch("x86_64-pc-linux-gnux32"),
            Some((
                Architecture::X86_64X32,
                Endianness::Little,
                PointerWidth::Bits32
            ))
        );
        assert_eq!(
            arch("powerpc64-unknown-linux-gnu"),
            Some((
                Architecture::PowerPc64,
                Endianness::Big,
                PointerWidth::Bits64
            ))
        );
        assert_eq!(arch("wasm32-unknown-unknown"), None);
        assert_eq!(arch(""), None);
    }

    /// A bitstream writer, for building test inputs.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        bit: usize,
    }

    impl Writer {
        fn fixed(&mut self, value: u64, width: u32) {
            for i in 0..width {
                if self.bit.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if (value >> i) & 1 == 1 {
                    *self.bytes.last_mut().unwrap() |= 1 << (self.bit % 8);
                }
                self.bit += 1;
            }
        }
        fn vbr(&mut self, mut value: u64, width: u32) {
            let high = 1u64 << (width - 1);
            loop {
                if value < high {
                    self.fixed(value, width);
                    return;
                }
                self.fixed((value & (high - 1)) | high, width);
                value >>= width - 1;
            }
        }
        fn align32(&mut self) {
            while !self.bit.is_multiple_of(32) {
                self.fixed(0, 1);
            }
        }
        /// A top-level block holding one abbreviated blob record, preceded
        /// by an unabbreviated record and a nested block to skip.
        fn blob_block(&mut self, id: u64, blob: &[u8]) {
            self.fixed(ENTER_SUBBLOCK, 2);
            self.vbr(id, 8);
            self.vbr(3, 4);
            self.align32();
            let length_at = self.bytes.len();
            self.fixed(0, 32);
            let body = self.bit;
            // an unabbreviated record [7, 1, 2]
            self.fixed(UNABBREV_RECORD, 3);
            self.vbr(7, 6);
            self.vbr(2, 6);
            self.vbr(1, 6);
            self.vbr(2, 6);
            // an empty nested block
            self.fixed(ENTER_SUBBLOCK, 3);
            self.vbr(99, 8);
            self.vbr(2, 4);
            self.align32();
            self.fixed(1, 32);
            self.fixed(END_BLOCK, 2);
            self.align32();
            // abbreviation 4: [literal 1, blob]
            self.fixed(DEFINE_ABBREV, 3);
            self.vbr(2, 5);
            self.fixed(1, 1);
            self.vbr(BLOB_RECORD, 8);
            self.fixed(0, 1);
            self.fixed(5, 3);
            self.fixed(4, 3);
            self.vbr(blob.len() as u64, 6);
            self.align32();
            for &byte in blob {
                self.fixed(u64::from(byte), 8);
            }
            self.align32();
            self.fixed(END_BLOCK, 3);
            self.align32();
            let words = u32::try_from((self.bit - body) / 32).unwrap();
            self.bytes[length_at..length_at + 4].copy_from_slice(&words.to_le_bytes());
        }
    }

    fn bitcode(triple: &str) -> Vec<u8> {
        let mut symtab = vec![0u8; 64];
        symtab[TRIPLE_FIELD..TRIPLE_FIELD + 4].copy_from_slice(&3u32.to_le_bytes());
        let size = u32::try_from(triple.len()).unwrap();
        symtab[TRIPLE_FIELD + 4..TRIPLE_FIELD + 8].copy_from_slice(&size.to_le_bytes());
        let strtab = format!("abc{triple}xyz");
        let mut writer = Writer::default();
        for &byte in &BITCODE_MAGIC {
            writer.fixed(u64::from(byte), 8);
        }
        writer.blob_block(13, b"skipped");
        writer.blob_block(SYMTAB_BLOCK_ID, &symtab);
        writer.blob_block(STRTAB_BLOCK_ID, strtab.as_bytes());
        writer.bytes
    }

    #[test]
    fn reads_the_symbol_table_triple() {
        let data = bitcode("i686-unknown-linux-gnu");
        assert_eq!(bitcode_triple(&data), Some(&b"i686-unknown-linux-gnu"[..]));
        assert_eq!(of_bitcode(&data).map(|t| t.arch), Some(Architecture::X86));
        // in a wrapper header, followed by padding
        let mut wrapped = Vec::new();
        for word in [WRAPPER_MAGIC, 0, 20, u32::try_from(data.len()).unwrap(), 7] {
            wrapped.extend_from_slice(&word.to_le_bytes());
        }
        wrapped.extend_from_slice(&data);
        wrapped.extend_from_slice(&[0; 12]);
        assert_eq!(
            of_bitcode(&wrapped).map(|t| t.arch),
            Some(Architecture::X86)
        );
    }

    #[test]
    fn malformed_bitcode_is_none() {
        let data = bitcode("aarch64-unknown-linux-gnu");
        assert_eq!(of_bitcode(b"BC\xc0\xde"), None);
        assert_eq!(of_bitcode(b"not bitcode"), None);
        for len in 0..data.len() {
            // every truncation is rejected or reads the whole triple
            let found = bitcode_triple(&data[..len]);
            assert!(found.is_none() || found == Some(&b"aarch64-unknown-linux-gnu"[..]));
        }
        for i in 4..data.len() {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut bad = data.clone();
                bad[i] ^= flip;
                let _ = of_bitcode(&bad);
            }
        }
    }
}
