//! Objective-C selector stubs (`_objc_msgSend$<selector>`).
//!
//! Apple clang (for macOS 13 and later) calls `_objc_msgSend$foo` instead of
//! loading the `foo` selector and calling `_objc_msgSend`, leaving the
//! linker to define those symbols, as ld64 and lld do. qld generates them
//! as an ordinary relocatable object, so the rest of the link (layout,
//! relocations, dead stripping, fixups) treats them like any input:
//!
//! - `__TEXT,__objc_stubs`: one stub per selector, which loads the selector
//!   reference and jumps to `_objc_msgSend` through `__got`. On arm64,
//!   `adrp x1; ldr x1; adrp x16; ldr x16; br x16` and three `brk #1` (32
//!   bytes); on x86_64, `movq sel(%rip), %rsi; jmp *_objc_msgSend@GOT(%rip)`
//!   (13 bytes). These are ld64's "fast" stubs, aligned as lld aligns them.
//! - `__DATA,__objc_selrefs`: one selector reference per stub, which the
//!   Objective-C runtime uniques at load time.
//! - `__TEXT,__objc_methname`: the selector names.
//!
//! The stubs are private externs, so they are neither exported nor clash
//! with another image's.

#![deny(clippy::arithmetic_side_effects)]

use crate::macho::read::Arch;
use crate::macho::read::consts::{
    ARM64_RELOC_GOT_LOAD_PAGE21, ARM64_RELOC_GOT_LOAD_PAGEOFF12, ARM64_RELOC_PAGE21,
    ARM64_RELOC_PAGEOFF12, ARM64_RELOC_UNSIGNED, CPU_TYPE_ARM64, LC_SEGMENT_64, LC_SYMTAB,
    MH_MAGIC_64, MH_OBJECT, MH_SUBSECTIONS_VIA_SYMBOLS, N_EXT, N_PEXT, N_SECT, N_UNDF,
    S_ATTR_PURE_INSTRUCTIONS, S_ATTR_SOME_INSTRUCTIONS, S_CSTRING_LITERALS, S_LITERAL_POINTERS,
    X86_64_RELOC_GOT, X86_64_RELOC_SIGNED, X86_64_RELOC_UNSIGNED,
};

use super::buf::{pad_to, push_name16, push16, push32, push64, to_u64};

/// The prefix of the symbols a selector stub defines.
pub const PREFIX: &[u8] = b"_objc_msgSend$";

/// A `section_64` header of the generated object.
struct SectionHeader {
    sectname: &'static [u8],
    segname: &'static [u8],
    addr: u64,
    size: u64,
    offset: u32,
    align: u32,
    reloff: u32,
    nreloc: u32,
    flags: u32,
}

fn relocation(
    out: &mut Vec<u8>,
    address: u32,
    symbol: u32,
    pcrel: bool,
    extern_: bool,
    r_type: u8,
) {
    push32(out, address);
    let word = (symbol & 0x00ff_ffff)
        | (u32::from(pcrel) << 24)
        | (2 << 25)
        | (u32::from(extern_) << 27)
        | (u32::from(r_type) << 28);
    push32(out, word);
}

fn relocation64(out: &mut Vec<u8>, address: u32, symbol: u32, r_type: u8) {
    push32(out, address);
    let word = (symbol & 0x00ff_ffff) | (3 << 25) | (u32::from(r_type) << 28);
    push32(out, word);
}

/// Builds the object defining the stubs of `selectors` (names without the
/// prefix, sorted and unique) for `arch`.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn object(arch: Arch, selectors: &[Vec<u8>]) -> Vec<u8> {
    let arm64 = arch.cpu_type == CPU_TYPE_ARM64;
    let count = u32::try_from(selectors.len()).unwrap_or(0);
    let stub_size: u32 = if arm64 { 32 } else { 13 };

    // Section contents, at object addresses: stubs at 0, then selector
    // references, then names.
    let stubs_size = count.saturating_mul(stub_size);
    let selrefs_addr = u64::from(stubs_size).next_multiple_of(8);
    let selrefs_size = u64::from(count).saturating_mul(8);
    let names_addr = selrefs_addr.saturating_add(selrefs_size);
    let mut names = Vec::new();
    let mut name_offsets = Vec::new();
    for selector in selectors {
        name_offsets.push(to_u64(names.len()));
        names.extend_from_slice(selector);
        names.push(0);
    }

    // Symbols: local selector references first (0..count), then the stubs
    // (count..2*count), then _objc_msgSend.
    let mut strings = vec![0u8];
    let mut symtab = Vec::new();
    let add_string = |strings: &mut Vec<u8>, name: &[u8]| {
        let offset = u32::try_from(strings.len()).unwrap_or(0);
        strings.extend_from_slice(name);
        strings.push(0);
        offset
    };
    for (index, _) in selectors.iter().enumerate() {
        let name = format!("lSelectorRef{index}");
        let strx = add_string(&mut strings, name.as_bytes());
        push32(&mut symtab, strx);
        symtab.push(N_SECT);
        symtab.push(2);
        push16(&mut symtab, 0);
        push64(
            &mut symtab,
            selrefs_addr.saturating_add(to_u64(index).saturating_mul(8)),
        );
    }
    for (index, selector) in selectors.iter().enumerate() {
        let mut name = PREFIX.to_vec();
        name.extend_from_slice(selector);
        let strx = add_string(&mut strings, &name);
        push32(&mut symtab, strx);
        symtab.push(N_SECT | N_EXT | N_PEXT);
        symtab.push(1);
        push16(&mut symtab, 0);
        push64(
            &mut symtab,
            to_u64(index).saturating_mul(u64::from(stub_size)),
        );
    }
    let msgsend = count.saturating_mul(2);
    let strx = add_string(&mut strings, b"_objc_msgSend");
    push32(&mut symtab, strx);
    symtab.push(N_UNDF | N_EXT);
    symtab.push(0);
    push16(&mut symtab, 0);
    push64(&mut symtab, 0);
    pad_to(&mut strings, 8);

    let mut code = Vec::new();
    let mut code_relocs = Vec::new();
    for index in 0..count {
        let base = index.saturating_mul(stub_size);
        if arm64 {
            for insn in [
                0x9000_0001u32, // adrp x1, selref@PAGE
                0xf940_0021,    // ldr x1, [x1, selref@PAGEOFF]
                0x9000_0010,    // adrp x16, _objc_msgSend@GOTPAGE
                0xf940_0210,    // ldr x16, [x16, _objc_msgSend@GOTPAGEOFF]
                0xd61f_0200,    // br x16
                0xd420_0020,    // brk #1
                0xd420_0020,
                0xd420_0020,
            ] {
                push32(&mut code, insn);
            }
            relocation(
                &mut code_relocs,
                base,
                index,
                true,
                true,
                ARM64_RELOC_PAGE21,
            );
            relocation(
                &mut code_relocs,
                base.saturating_add(4),
                index,
                false,
                true,
                ARM64_RELOC_PAGEOFF12,
            );
            relocation(
                &mut code_relocs,
                base.saturating_add(8),
                msgsend,
                true,
                true,
                ARM64_RELOC_GOT_LOAD_PAGE21,
            );
            relocation(
                &mut code_relocs,
                base.saturating_add(12),
                msgsend,
                false,
                true,
                ARM64_RELOC_GOT_LOAD_PAGEOFF12,
            );
        } else {
            code.extend_from_slice(&[
                0x48, 0x8b, 0x35, 0, 0, 0, 0, // movq selref(%rip), %rsi
                0xff, 0x25, 0, 0, 0, 0, // jmp *_objc_msgSend@GOTPCREL(%rip)
            ]);
            relocation(
                &mut code_relocs,
                base.saturating_add(3),
                index,
                true,
                true,
                X86_64_RELOC_SIGNED,
            );
            relocation(
                &mut code_relocs,
                base.saturating_add(9),
                msgsend,
                true,
                true,
                X86_64_RELOC_GOT,
            );
        }
    }

    let mut selrefs = Vec::new();
    let mut selref_relocs = Vec::new();
    let unsigned = if arm64 {
        ARM64_RELOC_UNSIGNED
    } else {
        X86_64_RELOC_UNSIGNED
    };
    for (index, offset) in name_offsets.iter().enumerate() {
        push64(&mut selrefs, names_addr.saturating_add(*offset));
        // Section-relative, against section 3 (the names).
        relocation64(
            &mut selref_relocs,
            u32::try_from(index.saturating_mul(8)).unwrap_or(0),
            3,
            unsigned,
        );
    }

    // Layout of the file: header, commands, contents, relocations, symbols.
    let header_size = 32u32;
    let segment_size = 72u32.saturating_add(3u32.saturating_mul(80));
    let commands_size = segment_size.saturating_add(24);
    let contents = header_size.saturating_add(commands_size);
    let stubs_off = contents;
    let selrefs_off = stubs_off.saturating_add(u32::try_from(selrefs_addr).unwrap_or(0));
    let names_off = stubs_off.saturating_add(u32::try_from(names_addr).unwrap_or(0));
    let relocs_off = names_off
        .saturating_add(u32::try_from(names.len()).unwrap_or(0))
        .next_multiple_of(8);
    let code_relocs_off = relocs_off;
    let selref_relocs_off =
        code_relocs_off.saturating_add(u32::try_from(code_relocs.len()).unwrap_or(0));
    let symoff = selref_relocs_off.saturating_add(u32::try_from(selref_relocs.len()).unwrap_or(0));
    let stroff = symoff.saturating_add(u32::try_from(symtab.len()).unwrap_or(0));

    let mut out = Vec::new();
    push32(&mut out, MH_MAGIC_64);
    push32(&mut out, arch.cpu_type);
    push32(&mut out, arch.cpu_subtype);
    push32(&mut out, MH_OBJECT);
    push32(&mut out, 2);
    push32(&mut out, commands_size);
    push32(&mut out, MH_SUBSECTIONS_VIA_SYMBOLS);
    push32(&mut out, 0);

    push32(&mut out, LC_SEGMENT_64);
    push32(&mut out, segment_size);
    push_name16(&mut out, b"");
    push64(&mut out, 0);
    let vmsize = names_addr.saturating_add(to_u64(names.len()));
    push64(&mut out, vmsize);
    push64(&mut out, u64::from(contents));
    push64(&mut out, vmsize);
    push32(&mut out, 7);
    push32(&mut out, 7);
    push32(&mut out, 3);
    push32(&mut out, 0);
    let sections = [
        SectionHeader {
            sectname: b"__objc_stubs",
            segname: b"__TEXT",
            addr: 0,
            size: u64::from(stubs_size),
            offset: stubs_off,
            align: if arm64 { 5 } else { 0 },
            reloff: code_relocs_off,
            nreloc: u32::try_from(code_relocs.len() / 8).unwrap_or(0),
            flags: S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS,
        },
        SectionHeader {
            sectname: b"__objc_selrefs",
            segname: b"__DATA",
            addr: selrefs_addr,
            size: selrefs_size,
            offset: selrefs_off,
            align: 3,
            reloff: selref_relocs_off,
            nreloc: count,
            flags: S_LITERAL_POINTERS,
        },
        SectionHeader {
            sectname: b"__objc_methname",
            segname: b"__TEXT",
            addr: names_addr,
            size: to_u64(names.len()),
            offset: names_off,
            align: 0,
            reloff: 0,
            nreloc: 0,
            flags: S_CSTRING_LITERALS,
        },
    ];
    for section in sections {
        push_name16(&mut out, section.sectname);
        push_name16(&mut out, section.segname);
        push64(&mut out, section.addr);
        push64(&mut out, section.size);
        push32(&mut out, section.offset);
        push32(&mut out, section.align);
        push32(&mut out, section.reloff);
        push32(&mut out, section.nreloc);
        push32(&mut out, section.flags);
        push32(&mut out, 0);
        push32(&mut out, 0);
        push32(&mut out, 0);
    }
    push32(&mut out, LC_SYMTAB);
    push32(&mut out, 24);
    push32(&mut out, symoff);
    push32(&mut out, count.saturating_mul(2).saturating_add(1));
    push32(&mut out, stroff);
    push32(&mut out, u32::try_from(strings.len()).unwrap_or(0));

    let place = |out: &mut Vec<u8>, at: u32, bytes: &[u8]| {
        let at = usize::try_from(at).unwrap_or(usize::MAX);
        if out.len() < at {
            out.resize(at, 0);
        }
        out.extend_from_slice(bytes);
    };
    place(&mut out, stubs_off, &code);
    place(&mut out, selrefs_off, &selrefs);
    place(&mut out, names_off, &names);
    place(&mut out, code_relocs_off, &code_relocs);
    place(&mut out, selref_relocs_off, &selref_relocs);
    place(&mut out, symoff, &symtab);
    place(&mut out, stroff, &strings);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macho::read::{Atomization, ObjectFile, RelocationTarget, Source};
    use std::path::Path;

    #[test]
    fn parses_as_an_object() {
        for arch in [Arch::ARM64, Arch::X86_64] {
            let bytes = object(arch, &[b"answer".to_vec(), b"initWithCount:".to_vec()]);
            let object = ObjectFile::parse(&bytes, Source::new(Path::new("stubs.o"))).unwrap();
            assert_eq!(object.sections().len(), 3);
            let atoms = Atomization::new(&object).unwrap();
            assert_eq!(atoms.section_atoms(0).len(), 2);
            assert_eq!(atoms.section_atoms(1).len(), 2);
            assert_eq!(atoms.section_atoms(2).len(), 2);
            let names: Vec<Vec<u8>> = object
                .symbols()
                .iter()
                .map(|s| s.unwrap().name.to_vec())
                .collect();
            assert!(names.contains(&b"_objc_msgSend$initWithCount:".to_vec()));
            let selref = object.relocations(1).unwrap().get(1).unwrap();
            assert_eq!(selref.target, RelocationTarget::Section(3));
            let data = object.section_data(1).unwrap();
            assert_eq!(
                u64::from_le_bytes(data[8..16].try_into().unwrap()),
                object.sections()[2].addr + 7
            );
            for (index, _) in object.sections().iter().enumerate() {
                for relocation in object.paired_relocations(index).unwrap() {
                    relocation.unwrap();
                }
            }
        }
    }
}
