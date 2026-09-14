/* Entry point and syscall stubs for x86-64 Linux, without libc. */

__asm__(
    ".text\n"
    ".globl _start\n"
    ".type _start, @function\n"
    "_start:\n"
    "    xor %ebp, %ebp\n"
    "    and $-16, %rsp\n"
    "    call c_main\n"
    "    mov %eax, %edi\n"
    "    mov $60, %eax\n"
    "    syscall\n"
    "    hlt\n"
    ".globl sys_write\n"
    ".type sys_write, @function\n"
    "sys_write:\n"
    "    mov $1, %eax\n"
    "    syscall\n"
    "    ret\n");

long sys_write(long fd, const char *buf, unsigned long len);
const char *message(int index);

static unsigned long length(const char *s) {
    unsigned long n = 0;
    while (s[n])
        n++;
    return n;
}

int c_main(void) {
    for (int i = 0; message(i); i++)
        sys_write(1, message(i), length(message(i)));
    return 7;
}
