// Calls Objective-C selector stubs the way Apple clang emits them for
// macOS 13 (-fobjc-msgsend-selector-stubs, which upstream clang lacks):
// the linker must define _objc_msgSend$<selector>. Messages to nil return
// 0, so this runs without any class.
    .text
    .globl _main
    .p2align 2
_main:
    stp x29, x30, [sp, #-16]!
    mov x29, sp
    mov x0, #0
    bl _objc_msgSend$description
    mov x0, #0
    bl "_objc_msgSend$initWithCount:"
    mov x0, #0
    ldp x29, x30, [sp], #16
    ret

.subsections_via_symbols
