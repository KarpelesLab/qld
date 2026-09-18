//! The built-in x86-64 layout as a linker script.
//!
//! When the script engine runs without a `-T` script that replaces the
//! default layout (implicit scripts, `INSERT`, `-Ttext`, `-N`, ...), this
//! script is the base it works on. It follows GNU ld's `elf_x86_64` scripts
//! (`ld --verbose`), with the same variants: executables start at
//! `0x400000`, position-independent outputs at 0; `-z separate-code` puts
//! page boundaries around the code; `-N` and `-n` drop page alignment;
//! `-z now` moves `.got.plt` into `.got`. The section list and its patterns
//! are the ones [`crate::elf::rules::DEFAULT_RULES`] encodes.

use std::fmt::Write as _;

use crate::args::{LinkOptions, MagicMode, OutputKind, SeparateCode};

/// The default script text for `options`.
#[must_use]
pub fn default_script(options: &LinkOptions) -> String {
    let pic = matches!(
        options.kind,
        OutputKind::Pie | OutputKind::Shared | OutputKind::StaticPie
    );
    let shared = options.kind == OutputKind::Shared;
    let paged = options.magic == MagicMode::Normal;
    // -n keeps the data segment alignment; -N drops it.
    let data_segment = options.magic != MagicMode::Omagic;
    let separate =
        paged && options.separate_code.unwrap_or(SeparateCode::Code) != SeparateCode::None;
    let base = if pic { "0" } else { "0x400000" };
    let mut s = String::with_capacity(8192);
    let _ = writeln!(
        s,
        "OUTPUT_FORMAT(\"elf64-x86-64\")\nOUTPUT_ARCH(i386:x86-64)\nSECTIONS\n{{"
    );
    if !shared {
        let _ = writeln!(
            s,
            "PROVIDE (__executable_start = SEGMENT_START(\"text-segment\", {base}));"
        );
    }
    let _ = writeln!(
        s,
        ". = SEGMENT_START(\"text-segment\", {base}) + SIZEOF_HEADERS;"
    );
    s.push_str(".note.gnu.build-id : { *(.note.gnu.build-id) }\n");
    if !shared {
        s.push_str(".interp : { *(.interp) }\n");
    }
    s.push_str(
        ".hash : { *(.hash) }
.gnu.hash : { *(.gnu.hash) }
.dynsym : { *(.dynsym) }
.dynstr : { *(.dynstr) }
.gnu.version : { *(.gnu.version) }
.gnu.version_d : { *(.gnu.version_d) }
.gnu.version_r : { *(.gnu.version_r) }
",
    );
    let rela_groups: [(&str, &str); 17] = [
        (".rela.init", "*(.rela.init)"),
        (
            ".rela.text",
            "*(.rela.text .rela.text.* .rela.gnu.linkonce.t.*)",
        ),
        (".rela.fini", "*(.rela.fini)"),
        (
            ".rela.rodata",
            "*(.rela.rodata .rela.rodata.* .rela.gnu.linkonce.r.*)",
        ),
        (
            ".rela.data.rel.ro",
            "*(.rela.data.rel.ro .rela.data.rel.ro.* .rela.gnu.linkonce.d.rel.ro.*)",
        ),
        (
            ".rela.data",
            "*(.rela.data .rela.data.* .rela.gnu.linkonce.d.*)",
        ),
        (
            ".rela.tdata",
            "*(.rela.tdata .rela.tdata.* .rela.gnu.linkonce.td.*)",
        ),
        (
            ".rela.tbss",
            "*(.rela.tbss .rela.tbss.* .rela.gnu.linkonce.tb.*)",
        ),
        (".rela.ctors", "*(.rela.ctors)"),
        (".rela.dtors", "*(.rela.dtors)"),
        (".rela.got", "*(.rela.got)"),
        (".rela.tls", "*(.rela.tls)"),
        (
            ".rela.bss",
            "*(.rela.bss .rela.bss.* .rela.gnu.linkonce.b.*)",
        ),
        (
            ".rela.ldata",
            "*(.rela.ldata .rela.ldata.* .rela.gnu.linkonce.l.*)",
        ),
        (
            ".rela.lbss",
            "*(.rela.lbss .rela.lbss.* .rela.gnu.linkonce.lb.*)",
        ),
        (
            ".rela.lrodata",
            "*(.rela.lrodata .rela.lrodata.* .rela.gnu.linkonce.lr.*)",
        ),
        (".rela.ifunc", "*(.rela.ifunc)"),
    ];
    if paged {
        s.push_str(".rela.dyn : {\n");
        for (name, patterns) in rela_groups {
            if name != ".rela.data.rel.ro" {
                let _ = writeln!(s, "  {patterns}");
            }
        }
        s.push_str("}\n");
    } else {
        for (name, patterns) in rela_groups {
            let _ = writeln!(s, "{name} : {{ {patterns} }}");
        }
    }
    if pic {
        s.push_str(".rela.plt : { *(.rela.plt) *(.rela.iplt) }\n");
    } else {
        s.push_str(
            ".rela.plt : { *(.rela.plt) PROVIDE_HIDDEN (__rela_iplt_start = .); *(.rela.iplt) PROVIDE_HIDDEN (__rela_iplt_end = .); }\n",
        );
    }
    s.push_str(".relr.dyn : { *(.relr.dyn) }\n");
    if separate {
        s.push_str(". = ALIGN(CONSTANT (MAXPAGESIZE));\n");
    }
    s.push_str(
        ".init : { KEEP (*(SORT_NONE(.init))) }
.plt : { *(.plt) *(.iplt) }
.plt.got : { *(.plt.got) }
.plt.sec : { *(.plt.sec) }
.text : {
  *(.text.unlikely .text.*_unlikely .text.unlikely.*)
  *(.text.exit .text.exit.*)
  *(.text.startup .text.startup.*)
  *(.text.hot .text.hot.*)
  *(SORT(.text.sorted.*))
  *(.text .stub .text.* .gnu.linkonce.t.*)
  *(.gnu.warning)
}
.fini : { KEEP (*(SORT_NONE(.fini))) }
PROVIDE (__etext = .);
PROVIDE (_etext = .);
PROVIDE (etext = .);
",
    );
    if separate {
        s.push_str(
            ". = ALIGN(CONSTANT (MAXPAGESIZE));
. = SEGMENT_START(\"rodata-segment\", ALIGN(CONSTANT (MAXPAGESIZE)) + (. & (CONSTANT (MAXPAGESIZE) - 1)));
",
        );
    }
    s.push_str(
        ".rodata : { *(.rodata .rodata.* .gnu.linkonce.r.*) }
.rodata1 : { *(.rodata1) }
.eh_frame_hdr : { *(.eh_frame_hdr) *(.eh_frame_entry .eh_frame_entry.*) }
.eh_frame : ONLY_IF_RO { KEEP (*(.eh_frame)) *(.eh_frame.*) }
.sframe : ONLY_IF_RO { KEEP (*(.sframe)) *(.sframe.*) }
.gcc_except_table : ONLY_IF_RO { *(.gcc_except_table .gcc_except_table.*) }
.gnu_extab : ONLY_IF_RO { *(.gnu_extab*) }
.exception_ranges : ONLY_IF_RO { *(.exception_ranges*) }
.note.build-id : { *(.note.build-id) }
.note.GNU-stack : { *(.note.GNU-stack) }
.note.gnu.property : { *(.note.gnu.property) }
.note.ABI-tag : { *(.note.ABI-tag) }
.note.package : { *(.note.package) }
.note.dlopen : { *(.note.dlopen) }
.note.netbsd.ident : { *(.note.netbsd.ident) }
.note.openbsd.ident : { *(.note.openbsd.ident) }
",
    );
    if data_segment {
        s.push_str(". = DATA_SEGMENT_ALIGN (CONSTANT (MAXPAGESIZE), CONSTANT (COMMONPAGESIZE));\n");
    } else {
        s.push_str(". = .;\n");
    }
    s.push_str(
        ".eh_frame : ONLY_IF_RW { KEEP (*(.eh_frame)) *(.eh_frame.*) }
.sframe : ONLY_IF_RW { KEEP (*(.sframe)) *(.sframe.*) }
.gnu_extab : ONLY_IF_RW { *(.gnu_extab) }
.gcc_except_table : ONLY_IF_RW { *(.gcc_except_table .gcc_except_table.*) }
.exception_ranges : ONLY_IF_RW { *(.exception_ranges*) }
",
    );
    let hidden = |s: &mut String, name: &str| {
        if !shared {
            let _ = write!(s, "PROVIDE_HIDDEN ({name} = .); ");
        }
    };
    s.push_str(".tdata : { ");
    hidden(&mut s, "__tdata_start");
    s.push_str("*(.tdata .tdata.* .gnu.linkonce.td.*) }\n");
    s.push_str(".tbss : { *(.tbss .tbss.* .gnu.linkonce.tb.*) *(.tcommon) }\n");
    s.push_str(".preinit_array : { ");
    hidden(&mut s, "__preinit_array_start");
    s.push_str("KEEP (*(.preinit_array)) ");
    hidden(&mut s, "__preinit_array_end");
    s.push_str("}\n.init_array : { ");
    hidden(&mut s, "__init_array_start");
    s.push_str(
        "KEEP (*(SORT_BY_INIT_PRIORITY(.init_array.*) SORT_BY_INIT_PRIORITY(.ctors.*))) \
         KEEP (*(.init_array EXCLUDE_FILE (*crtbegin.o *crtbegin?.o *crtend.o *crtend?.o ) .ctors)) ",
    );
    hidden(&mut s, "__init_array_end");
    s.push_str("}\n.fini_array : { ");
    hidden(&mut s, "__fini_array_start");
    s.push_str(
        "KEEP (*(SORT_BY_INIT_PRIORITY(.fini_array.*) SORT_BY_INIT_PRIORITY(.dtors.*))) \
         KEEP (*(.fini_array EXCLUDE_FILE (*crtbegin.o *crtbegin?.o *crtend.o *crtend?.o ) .dtors)) ",
    );
    hidden(&mut s, "__fini_array_end");
    s.push_str("}\n");
    s.push_str(
        ".ctors : {
  KEEP (*crtbegin.o(.ctors))
  KEEP (*crtbegin?.o(.ctors))
  KEEP (*(EXCLUDE_FILE (*crtend.o *crtend?.o ) .ctors))
  KEEP (*(SORT(.ctors.*)))
  KEEP (*(.ctors))
}
.dtors : {
  KEEP (*crtbegin.o(.dtors))
  KEEP (*crtbegin?.o(.dtors))
  KEEP (*(EXCLUDE_FILE (*crtend.o *crtend?.o ) .dtors))
  KEEP (*(SORT(.dtors.*)))
  KEEP (*(.dtors))
}
.jcr : { KEEP (*(.jcr)) }
.data.rel.ro : { *(.data.rel.ro.local* .gnu.linkonce.d.rel.ro.local.*) *(.data.rel.ro .data.rel.ro.* .gnu.linkonce.d.rel.ro.*) }
.dynamic : { *(.dynamic) }
",
    );
    if options.bind_now && options.relro {
        s.push_str(".got : { *(.got.plt) *(.igot.plt) *(.got) *(.igot) }\n");
        if data_segment {
            s.push_str(". = DATA_SEGMENT_RELRO_END (0, .);\n");
        }
    } else {
        s.push_str(".got : { *(.got) *(.igot) }\n");
        if data_segment {
            s.push_str(". = DATA_SEGMENT_RELRO_END (SIZEOF (.got.plt) >= 24 ? 24 : 0, .);\n");
        }
        s.push_str(".got.plt : { *(.got.plt) *(.igot.plt) }\n");
    }
    s.push_str(
        ".data : { *(.data .data.* .gnu.linkonce.d.*) SORT(CONSTRUCTORS) }
.data1 : { *(.data1) }
",
    );
    if shared {
        s.push_str("PROVIDE (_edata = .);\n");
    } else {
        s.push_str("_edata = .;\n");
    }
    s.push_str("PROVIDE (edata = .);\n. = ALIGN(ALIGNOF(NEXT_SECTION));\n");
    if shared {
        s.push_str("PROVIDE (__bss_start = .);\n");
    } else {
        s.push_str("__bss_start = .;\n");
    }
    s.push_str(
        ".bss : {
  *(.dynbss)
  *(.bss .bss.* .gnu.linkonce.b.*)
  *(COMMON)
  . = ALIGN(. != 0 ? 64 / 8 : 1);
}
.lbss : {
  *(.dynlbss)
  *(.lbss .lbss.* .gnu.linkonce.lb.*)
  *(LARGE_COMMON)
}
. = ALIGN(64 / 8);
. = SEGMENT_START(\"ldata-segment\", .);
",
    );
    s.push_str(
        ".lrodata ALIGN(CONSTANT (MAXPAGESIZE)) + (. & (CONSTANT (MAXPAGESIZE) - 1)) : { *(.lrodata .lrodata.* .gnu.linkonce.lr.*) }
.ldata ALIGN(CONSTANT (MAXPAGESIZE)) + (. & (CONSTANT (MAXPAGESIZE) - 1)) : { *(.ldata .ldata.* .gnu.linkonce.l.*) . = ALIGN(. != 0 ? 64 / 8 : 1); }
",
    );
    s.push_str(". = ALIGN(64 / 8);\n");
    if shared {
        s.push_str("PROVIDE (_end = .);\n");
    } else {
        s.push_str("_end = .;\n");
    }
    s.push_str("PROVIDE (end = .);\n");
    if data_segment {
        s.push_str(". = DATA_SEGMENT_END (.);\n");
    }
    s.push_str(
        ".stab 0 : { *(.stab) }
.stabstr 0 : { *(.stabstr) }
.stab.excl 0 : { *(.stab.excl) }
.stab.exclstr 0 : { *(.stab.exclstr) }
.stab.index 0 : { *(.stab.index) }
.stab.indexstr 0 : { *(.stab.indexstr) }
.comment 0 (INFO) : { *(.comment); LINKER_VERSION; }
.gnu.build.attributes : { *(.gnu.build.attributes .gnu.build.attributes.*) }
",
    );
    for name in [
        ".debug",
        ".line",
        ".debug_srcinfo",
        ".debug_sfnames",
        ".debug_aranges",
        ".debug_pubnames",
    ] {
        let _ = writeln!(s, "{name} 0 : {{ *({name}) }}");
    }
    s.push_str(".debug_info 0 : { *(.debug_info .gnu.linkonce.wi.*) }\n");
    s.push_str(".debug_abbrev 0 : { *(.debug_abbrev) }\n");
    s.push_str(".debug_line 0 : { *(.debug_line .debug_line.* .debug_line_end) }\n");
    for name in [
        ".debug_frame",
        ".debug_str",
        ".debug_loc",
        ".debug_macinfo",
        ".debug_weaknames",
        ".debug_funcnames",
        ".debug_typenames",
        ".debug_varnames",
        ".debug_pubtypes",
        ".debug_ranges",
        ".debug_addr",
        ".debug_line_str",
        ".debug_loclists",
        ".debug_macro",
        ".debug_names",
        ".debug_rnglists",
        ".debug_str_offsets",
        ".debug_sup",
    ] {
        let _ = writeln!(s, "{name} 0 : {{ *({name}) }}");
    }
    s.push_str(
        ".gnu.attributes 0 : { KEEP (*(.gnu.attributes)) }
/DISCARD/ : { *(.note.GNU-stack) *(.gnu_debuglink) *(.gnu.lto_*) *(.gnu_object_only) }
}
",
    );
    s
}

/// The built-in layout of x86-64 relocatable output (`-r`), after GNU
/// ld's `elf_x86_64.xr`: every output section at address 0, in its order,
/// with `-r`'s narrower patterns (`.text.*` sections stay apart); what it
/// does not name is an orphan, written after it in input order.
#[must_use]
pub fn relocatable_script() -> &'static str {
    "SECTIONS
{
.note.gnu.build-id 0 : { *(.note.gnu.build-id) }
.interp 0 : { *(.interp) }
.hash 0 : { *(.hash) }
.gnu.hash 0 : { *(.gnu.hash) }
.dynsym 0 : { *(.dynsym) }
.dynstr 0 : { *(.dynstr) }
.gnu.version 0 : { *(.gnu.version) }
.gnu.version_d 0 : { *(.gnu.version_d) }
.gnu.version_r 0 : { *(.gnu.version_r) }
.rela.init 0 : { *(.rela.init) }
.rela.text 0 : { *(.rela.text) }
.rela.fini 0 : { *(.rela.fini) }
.rela.rodata 0 : { *(.rela.rodata) }
.rela.data.rel.ro 0 : { *(.rela.data.rel.ro) }
.rela.data 0 : { *(.rela.data) }
.rela.tdata 0 : { *(.rela.tdata) }
.rela.tbss 0 : { *(.rela.tbss) }
.rela.ctors 0 : { *(.rela.ctors) }
.rela.dtors 0 : { *(.rela.dtors) }
.rela.got 0 : { *(.rela.got) }
.rela.tls 0 : { *(.rela.tls) }
.rela.bss 0 : { *(.rela.bss) }
.rela.ldata 0 : { *(.rela.ldata) }
.rela.lbss 0 : { *(.rela.lbss) }
.rela.lrodata 0 : { *(.rela.lrodata) }
.rela.ifunc 0 : { *(.rela.ifunc) }
.rela.plt 0 : { *(.rela.plt) }
.relr.dyn : { *(.relr.dyn) }
.init 0 : { KEEP (*(SORT_NONE(.init))) }
.plt 0 : { *(.plt) }
.plt.got 0 : { *(.plt.got) }
.plt.sec 0 : { *(.plt.sec) }
.text 0 : { *(.text .stub) *(.gnu.warning) }
.fini 0 : { KEEP (*(SORT_NONE(.fini))) }
.rodata 0 : { *(.rodata) }
.rodata1 0 : { *(.rodata1) }
.eh_frame_hdr 0 : { *(.eh_frame_hdr) }
.eh_frame 0 : ONLY_IF_RO { KEEP (*(.eh_frame)) }
.sframe 0 : ONLY_IF_RO { KEEP (*(.sframe)) }
.gcc_except_table 0 : ONLY_IF_RO { *(.gcc_except_table) }
.gnu_extab 0 : ONLY_IF_RO { *(.gnu_extab*) }
.exception_ranges 0 : ONLY_IF_RO { *(.exception_ranges) }
.eh_frame 0 : ONLY_IF_RW { KEEP (*(.eh_frame)) }
.sframe 0 : ONLY_IF_RW { KEEP (*(.sframe)) }
.gnu_extab 0 : ONLY_IF_RW { *(.gnu_extab) }
.gcc_except_table 0 : ONLY_IF_RW { *(.gcc_except_table) }
.exception_ranges 0 : ONLY_IF_RW { *(.exception_ranges) }
.tdata 0 : { *(.tdata) }
.tbss 0 : { *(.tbss) }
.jcr 0 : { KEEP (*(.jcr)) }
.dynamic 0 : { *(.dynamic) }
.got 0 : { *(.got) }
.got.plt 0 : { *(.got.plt) }
.data 0 : { *(.data) }
.data1 0 : { *(.data1) }
.bss 0 : { *(.bss) }
.lbss 0 : { *(.lbss) }
.lrodata 0 : { *(.lrodata) }
.ldata 0 : { *(.ldata) }
.stab 0 : { *(.stab) }
.stabstr 0 : { *(.stabstr) }
.stab.excl 0 : { *(.stab.excl) }
.stab.exclstr 0 : { *(.stab.exclstr) }
.stab.index 0 : { *(.stab.index) }
.stab.indexstr 0 : { *(.stab.indexstr) }
.comment 0 (INFO) : { *(.comment) }
.gnu.build.attributes : { *(.gnu.build.attributes) }
.debug 0 : { *(.debug) }
.line 0 : { *(.line) }
.debug_srcinfo 0 : { *(.debug_srcinfo) }
.debug_sfnames 0 : { *(.debug_sfnames) }
.debug_aranges 0 : { *(.debug_aranges) }
.debug_pubnames 0 : { *(.debug_pubnames) }
.debug_info 0 : { *(.debug_info) }
.debug_abbrev 0 : { *(.debug_abbrev) }
.debug_line 0 : { *(.debug_line) }
.debug_frame 0 : { *(.debug_frame) }
.debug_str 0 : { *(.debug_str) }
.debug_loc 0 : { *(.debug_loc) }
.debug_macinfo 0 : { *(.debug_macinfo) }
.debug_weaknames 0 : { *(.debug_weaknames) }
.debug_funcnames 0 : { *(.debug_funcnames) }
.debug_typenames 0 : { *(.debug_typenames) }
.debug_varnames 0 : { *(.debug_varnames) }
.debug_pubtypes 0 : { *(.debug_pubtypes) }
.debug_ranges 0 : { *(.debug_ranges) }
.debug_addr 0 : { *(.debug_addr) }
.debug_line_str 0 : { *(.debug_line_str) }
.debug_loclists 0 : { *(.debug_loclists) }
.debug_macro 0 : { *(.debug_macro) }
.debug_names 0 : { *(.debug_names) }
.debug_rnglists 0 : { *(.debug_rnglists) }
.debug_str_offsets 0 : { *(.debug_str_offsets) }
.debug_sup 0 : { *(.debug_sup) }
.gnu.attributes 0 : { KEEP (*(.gnu.attributes)) }
}
"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::{NoIncludes, parse_script};
    use std::path::Path;

    #[test]
    fn every_variant_parses() {
        for kind in [OutputKind::Executable, OutputKind::Pie, OutputKind::Shared] {
            for magic in [MagicMode::Normal, MagicMode::Nmagic, MagicMode::Omagic] {
                for now in [false, true] {
                    let mut options = LinkOptions::new();
                    options.kind = kind;
                    options.magic = magic;
                    options.bind_now = now;
                    let text = default_script(&options);
                    parse_script(text.as_bytes(), Path::new("default"), &mut NoIncludes)
                        .unwrap_or_else(|e| panic!("{e}\n{text}"));
                }
            }
        }
        parse_script(
            relocatable_script().as_bytes(),
            Path::new("default"),
            &mut NoIncludes,
        )
        .unwrap();
    }
}
