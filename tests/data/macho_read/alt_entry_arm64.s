// Mach-O reader fixture: alt-entry, data in code and linker options.
// Build: clang --target=arm64-apple-macos13 -c alt_entry_arm64.s
    .linker_option "-framework", "QldKit"
    .linker_option "-lqldasm"

    .text
    .globl _outer
    .p2align 2
_outer:
    mov x0, #1
    .globl _inner
    .alt_entry _inner
_inner:
    add x0, x0, #1
    b _table_user

    .p2align 2
_table_user:
    adr x1, Ljump_table
    ret
    .data_region jt32
Ljump_table:
    .long _outer - Ljump_table
    .long _inner - Ljump_table
    .end_data_region

    .globl _after_table
    .p2align 2
_after_table:
    adrp x2, _outer@PAGE
    add x2, x2, _outer@PAGEOFF + 8
    adrp x3, _external@GOTPAGE
    ldr x3, [x3, _external@GOTPAGEOFF]
    ret

    .data
    .p2align 3
_diff:
    .quad _after_table - _outer
    .quad _external + 16

    .section __TEXT,__literal16,16byte_literals
    .p2align 4
Lliteral16:
    .quad 1, 2
    .quad 3, 4

    .section __DATA,__mod_init_func,mod_init_funcs
    .p2align 3
    .quad _outer

    .section __DATA,__keep,regular,no_dead_strip
_kept_data:
    .long 42

    .subsections_via_symbols
