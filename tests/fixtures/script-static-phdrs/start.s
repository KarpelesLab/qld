# A freestanding Linux program whose layout comes from a linker script.
# It prints the message between two script symbols, walks a table of
# function pointers collected with KEEP(SORT(...)), and exits with a value
# computed from script symbols.
.text
.globl _start
_start:
  mov $1, %edi
  lea msg_start(%rip), %rsi
  mov $msg_size, %edx
  mov $1, %eax
  syscall

  xor %ebx, %ebx
  lea __table_start(%rip), %r12
  lea __table_end(%rip), %r13
1:
  cmp %r13, %r12
  je 2f
  call *(%r12)
  add %eax, %ebx
  add $8, %r12
  jmp 1b
2:
  # 3 table entries (1 + 2 + 4) plus the counter set by the script.
  add counter(%rip), %ebx
  mov %ebx, %edi
  mov $60, %eax
  syscall

.section .text.one,"ax"
one:
  mov $1, %eax
  ret
.section .text.two,"ax"
two:
  mov $2, %eax
  ret
.section .text.four,"ax"
four:
  mov $4, %eax
  ret

.section .table.c,"aw"
.quad four
.section .table.a,"aw"
.quad one
.section .table.b,"aw"
.quad two

.section .rodata.msg,"a"
.globl msg_start
msg_start:
.ascii "linked by a script\n"
.globl msg_end
msg_end:

.section .unused,"ax"
.globl never_called
never_called:
  ud2
