# A tiny ROM image: code, read-only data, and initialized data that runs
# from RAM but is stored in ROM.
.text
.globl _start
_start:
  mov $value, %eax
  mov (%rax), %eax
  ret

.section .rodata,"a"
.ascii "raw!"

.data
.globl value
value:
.long 0x11223344

.bss
.zero 32
