# Writes a file embedded with `-b binary` and exits with its size.
.text
.globl _start
_start:
  mov $1, %edi
  lea _binary_blob_txt_start(%rip), %rsi
  lea _binary_blob_txt_end(%rip), %rdx
  sub %rsi, %rdx
  mov $1, %eax
  syscall
  mov $_binary_blob_txt_size, %edi
  mov $60, %eax
  syscall
