//! Tiny x86-64 ELF relocatable objects and `ar` archives, built in memory.
//!
//! This stands in for a compiler or JIT that hands qld object code without
//! writing files. The examples and `tests/api.rs` include it with
//! `#[path = "support/objects.rs"] mod objects;`.
//!
//! The objects make a static program whose exit status is the value
//! [`answer_object`] returns:
//!
//! ```text
//! main.o:    _start: call answer; mov edi, eax; mov eax, 60; syscall
//! answer.o:  answer: mov eax, <value>; ret
//! ```

#![allow(dead_code)] // Each example uses a different part.

/// A symbol of [`object`]: defined at an offset in `.text`, or undefined.
pub struct Symbol<'a> {
    /// The name.
    pub name: &'a str,
    /// Offset in `.text`, or `None` for an undefined symbol.
    pub offset: Option<u64>,
}

/// A `.rela.text` entry of [`object`].
pub struct Reloc<'a> {
    /// Offset in `.text` of the field to relocate.
    pub offset: u64,
    /// The symbol it refers to, one of [`object`]'s symbols.
    pub symbol: &'a str,
    /// The `R_X86_64_*` type.
    pub kind: u32,
    /// The addend.
    pub addend: i64,
}

/// `R_X86_64_PLT32`.
pub const R_X86_64_PLT32: u32 = 4;

/// A relocatable x86-64 ELF object with one `.text` section, global
/// function symbols and relocations against them.
pub fn object(text: &[u8], symbols: &[Symbol<'_>], relocs: &[Reloc<'_>]) -> Vec<u8> {
    // Section numbers.
    const TEXT: u32 = 1;
    const SYMTAB: u32 = 4;
    const STRTAB: u32 = 5;
    const SHSTRTAB: u16 = 6;

    let mut shstrtab = vec![0u8];
    let name = |table: &mut Vec<u8>, text: &str| {
        let offset = table.len() as u32;
        table.extend_from_slice(text.as_bytes());
        table.push(0);
        offset
    };
    let names = [
        ".text",
        ".rela.text",
        ".note.GNU-stack",
        ".symtab",
        ".strtab",
        ".shstrtab",
    ]
    .map(|section| name(&mut shstrtab, section));

    let mut strtab = vec![0u8];
    let mut symtab = vec![0u8; 24]; // The null symbol.
    for symbol in symbols {
        let st_name = name(&mut strtab, symbol.name);
        let (info, shndx, value) = match symbol.offset {
            // STB_GLOBAL, STT_FUNC.
            Some(offset) => (0x12u8, TEXT as u16, offset),
            // STB_GLOBAL, STT_NOTYPE.
            None => (0x10, 0, 0),
        };
        symtab.extend_from_slice(&st_name.to_le_bytes());
        symtab.push(info);
        symtab.push(0); // STV_DEFAULT
        symtab.extend_from_slice(&shndx.to_le_bytes());
        symtab.extend_from_slice(&value.to_le_bytes());
        symtab.extend_from_slice(&0u64.to_le_bytes());
    }
    let mut rela = Vec::new();
    for reloc in relocs {
        let index = symbols
            .iter()
            .position(|symbol| symbol.name == reloc.symbol)
            .expect("relocation against an unknown symbol") as u64
            + 1;
        rela.extend_from_slice(&reloc.offset.to_le_bytes());
        rela.extend_from_slice(&(index << 32 | u64::from(reloc.kind)).to_le_bytes());
        rela.extend_from_slice(&reloc.addend.to_le_bytes());
    }

    // The contents follow the ELF header, each aligned to 8; the section
    // headers come last.
    let mut out = vec![0u8; 64];
    let place = |out: &mut Vec<u8>, bytes: &[u8]| {
        out.resize(out.len().next_multiple_of(8), 0);
        let offset = out.len() as u64;
        out.extend_from_slice(bytes);
        offset
    };
    let text_at = place(&mut out, text);
    let rela_at = place(&mut out, &rela);
    let note_at = place(&mut out, &[]);
    let symtab_at = place(&mut out, &symtab);
    let strtab_at = place(&mut out, &strtab);
    let shstrtab_at = place(&mut out, &shstrtab);
    out.resize(out.len().next_multiple_of(8), 0);
    let shoff = out.len() as u64;

    /// Name, type, flags, offset, size, link, info, alignment, entry size.
    type Header = (u32, u32, u64, u64, usize, u32, u32, u64, u64);
    let headers: [Header; 7] = [
        (0, 0, 0, 0, 0, 0, 0, 0, 0),
        (names[0], 1, 0x6, text_at, text.len(), 0, 0, 16, 0),
        (names[1], 4, 0x40, rela_at, rela.len(), SYMTAB, TEXT, 8, 24),
        (names[2], 1, 0, note_at, 0, 0, 0, 1, 0),
        (names[3], 2, 0, symtab_at, symtab.len(), STRTAB, 1, 8, 24),
        (names[4], 3, 0, strtab_at, strtab.len(), 0, 0, 1, 0),
        (names[5], 3, 0, shstrtab_at, shstrtab.len(), 0, 0, 1, 0),
    ];
    for (name, kind, flags, offset, size, link, info, align, entsize) in headers {
        out.extend_from_slice(&name.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // sh_addr
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&(size as u64).to_le_bytes());
        out.extend_from_slice(&link.to_le_bytes());
        out.extend_from_slice(&info.to_le_bytes());
        out.extend_from_slice(&align.to_le_bytes());
        out.extend_from_slice(&entsize.to_le_bytes());
    }

    let header = &mut out[..64];
    header[..16].copy_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    header[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
    header[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    header[20..24].copy_from_slice(&1u32.to_le_bytes()); // EV_CURRENT
    header[40..48].copy_from_slice(&shoff.to_le_bytes());
    header[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    header[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
    header[60..62].copy_from_slice(&(headers.len() as u16).to_le_bytes());
    header[62..64].copy_from_slice(&SHSTRTAB.to_le_bytes());
    out
}

/// `_start`: calls `answer` and exits with its result.
pub fn main_object() -> Vec<u8> {
    object(
        &[
            0xe8, 0, 0, 0, 0, // call answer
            0x89, 0xc7, // mov edi, eax
            0xb8, 60, 0, 0, 0, // mov eax, SYS_exit
            0x0f, 0x05, // syscall
        ],
        &[
            Symbol {
                name: "_start",
                offset: Some(0),
            },
            Symbol {
                name: "answer",
                offset: None,
            },
        ],
        &[Reloc {
            offset: 1,
            symbol: "answer",
            kind: R_X86_64_PLT32,
            addend: -4,
        }],
    )
}

/// `answer`: returns `value`.
pub fn answer_object(value: u8) -> Vec<u8> {
    object(
        &[0xb8, value, 0, 0, 0, 0xc3], // mov eax, value; ret
        &[Symbol {
            name: "answer",
            offset: Some(0),
        }],
        &[],
    )
}

/// A GNU `ar` archive of `members` (name, contents, defined symbols), with
/// a symbol index.
pub fn archive(members: &[(&str, &[u8], &[&str])]) -> Vec<u8> {
    fn header(out: &mut Vec<u8>, name: &str, size: usize) {
        let field = |text: String, width: usize| format!("{text:<width$}");
        let header = field(name.to_string(), 16)
            + &field("0".into(), 12)
            + &field("0".into(), 6)
            + &field("0".into(), 6)
            + &field("644".into(), 8)
            + &field(size.to_string(), 10)
            + "`\n";
        out.extend_from_slice(header.as_bytes());
    }
    let symbols: Vec<(usize, &str)> = members
        .iter()
        .enumerate()
        .flat_map(|(index, (_, _, names))| names.iter().map(move |name| (index, *name)))
        .collect();
    let names_size: usize = symbols.iter().map(|(_, name)| name.len() + 1).sum();
    let index_size = 4 + 4 * symbols.len() + names_size;
    let mut offset = 8 + 60 + index_size.next_multiple_of(2);
    let mut member_offsets = Vec::new();
    for (_, data, _) in members {
        member_offsets.push(offset as u32);
        offset += 60 + data.len().next_multiple_of(2);
    }

    let mut out = b"!<arch>\n".to_vec();
    header(&mut out, "/", index_size);
    out.extend_from_slice(&(symbols.len() as u32).to_be_bytes());
    for (member, _) in &symbols {
        out.extend_from_slice(&member_offsets[*member].to_be_bytes());
    }
    for (_, name) in &symbols {
        out.extend_from_slice(name.as_bytes());
        out.push(0);
    }
    if out.len() % 2 == 1 {
        out.push(b'\n');
    }
    for (name, data, _) in members {
        header(&mut out, &format!("{name}/"), data.len());
        out.extend_from_slice(data);
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

/// The entry point of an ELF executable image, if it is one.
pub fn entry_point(image: &[u8]) -> Option<u64> {
    if !image.starts_with(b"\x7fELF\x02\x01") {
        return None;
    }
    Some(u64::from_le_bytes(image.get(24..32)?.try_into().ok()?))
}
