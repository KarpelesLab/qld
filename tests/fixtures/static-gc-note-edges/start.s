# --gc-sections must follow relocations out of non-allocated note sections:
# SystemTap's .note.stapsdt references .stapsdt.base, and GNU ld and lld keep
# it. Debug sections, by contrast, must not keep code alive.
    .section .note.stapsdt,"?",@note
    .long 4
    .long 8
    .long 3
    .asciz "stap"
    .quad .stapsdt.base
    .section .stapsdt.base,"aG",@progbits,.stapsdt.base,comdat
    .byte 0
    .section .text.unused,"ax",@progbits
unused:
    ret
    .section .debug_foo,"",@progbits
    .quad unused
    .section .text._start,"ax",@progbits
    .globl _start
_start:
    mov $60, %eax
    mov $5, %edi
    syscall
