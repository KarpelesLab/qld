# Code that straddles a 64 KiB boundary, and data stored at a separate
# load address.
.text
.globl _start
_start:
  mov $0x12345678, %eax
  mov $0x9abcdef0, %ecx
  xor %ecx, %eax
  add $0x0f, %eax
  sub $0x01, %ecx
  ret
.ascii "boundary"

.data
.globl table
table:
.quad _start
.long 0xcafef00d
