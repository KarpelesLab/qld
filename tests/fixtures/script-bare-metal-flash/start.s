# Reset code for a Cortex-M style image on x86-64: a vector table, code
# that copies .data from its load address in flash and clears .bss.
.section .vectors,"a"
.globl vectors
vectors:
.quad reset_handler
.quad nmi_handler
.quad 0

.text
.globl reset_handler
reset_handler:
  mov $_sidata, %rsi
  mov $_sdata, %rdi
  mov $_edata, %rcx
  sub %rdi, %rcx
  rep movsb
  mov $_sbss, %rdi
  mov $_ebss, %rcx
  sub %rdi, %rcx
  xor %eax, %eax
  rep stosb
  call main
  hlt

.section .text.nmi,"ax"
.p2align 4
.globl nmi_handler
nmi_handler:
  iretq

.section .rodata
.globl message
message:
.asciz "hello, bare metal"

.data
.globl counter
.p2align 3
counter:
.quad 42

.bss
.globl buffer
buffer:
.zero 100
