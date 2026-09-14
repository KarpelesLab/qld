# Sums a table that a script collects into its own output section, adds a
# symbol from an implicit linker script, and exits with the result.
.text
.globl _start
_start:
  xor %eax, %eax
  lea __plugins_start(%rip), %rsi
  lea __plugins_end(%rip), %rdi
1:
  cmp %rdi, %rsi
  je 2f
  add (%rsi), %eax
  add $4, %rsi
  jmp 1b
2:
  add $answer, %eax
  mov %eax, %edi
  mov $60, %eax
  syscall

.section .plugins.b,"aw"
.long 20
.section .plugins.a,"aw"
.long 10

.data
.long 1

.bss
.zero 16
