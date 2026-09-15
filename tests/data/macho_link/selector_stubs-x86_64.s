# Calls Objective-C selector stubs the way Apple clang emits them for
# macOS 13 (-fobjc-msgsend-selector-stubs, which upstream clang lacks):
# the linker must define _objc_msgSend$<selector>. Messages to nil return
# 0, so this runs without any class.
    .text
    .globl _main
    .p2align 4
_main:
    pushq %rbp
    movq %rsp, %rbp
    xorl %edi, %edi
    callq _objc_msgSend$description
    xorl %edi, %edi
    callq "_objc_msgSend$initWithCount:"
    xorl %eax, %eax
    popq %rbp
    retq

.subsections_via_symbols
