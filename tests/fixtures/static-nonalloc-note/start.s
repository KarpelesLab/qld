# A non-allocated SHT_NOTE section, like glibc's SystemTap probes
# (.note.stapsdt), must keep its NOTE type in the output.
    .section .note.stapsdt,"?",@note
    .long 4
    .long 8
    .long 3
    .asciz "stap"
    .quad 0
    .section .stapsdt.base,"aG",@progbits,.stapsdt.base,comdat
    .byte 0
    .text
    .globl _start
_start:
    mov $60, %eax
    mov $3, %edi
    syscall
