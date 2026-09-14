# Application code, an init_array sorted by priority, and a large .bss.
.section .text.main,"ax"
.globl main
main:
  mov counter(%rip), %rax
  lea message(%rip), %rdx
  ret

.section .data.table,"aw"
.p2align 4
table:
.quad main, counter

.section .init_array.100,"aw"
.quad main
.section .init_array.5,"aw"
.quad main

.section .bss.big,"aw",@nobits
.p2align 6
big:
.zero 256
