# Reads values from .data, a named section and .bss, and exits with their
# sum. Absolute addressing checks that the command-line addresses hold.
.text
.globl _start
_start:
  movl $5, bss_value
  mov data_value, %eax
  add mysec_value, %eax
  add bss_value, %eax
  mov %eax, %edi
  mov $60, %eax
  syscall

.data
data_value:
.long 30

.section .mysec,"aw"
mysec_value:
.long 7

.bss
bss_value:
.zero 4
