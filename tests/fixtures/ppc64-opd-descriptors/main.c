/* A freestanding ELFv1 program: no libc, so the only thing between the
   kernel and the output is what the linker did. `_start`'s descriptor is
   `e_entry`, the calls into other.c go through that file's `.opd`
   descriptors, the message and the table are addressed TOC-relative, and
   a function pointer is a descriptor address the program dereferences
   itself. */

static long syscall4(long number, long a, long b, long c) {
    register long r0 __asm__("r0") = number;
    register long r3 __asm__("r3") = a;
    register long r4 __asm__("r4") = b;
    register long r5 __asm__("r5") = c;
    __asm__ volatile("sc"
                     : "+r"(r3)
                     : "r"(r0), "r"(r4), "r"(r5)
                     : "r6", "r7", "r8", "r9", "r10", "r11", "r12", "cr0",
                       "memory");
    return r3;
}

int add_qld(int a, int b);
int pick_qld(int index);
extern int table_qld[4];

static const char message_qld[] = "opd ok\n";

int (*indirect_qld)(int, int) = add_qld;

void _start(void) {
    int sum = add_qld(pick_qld(2), pick_qld(1));
    sum += indirect_qld(table_qld[0], table_qld[3]);
    if (sum == 10) {
        syscall4(4, 1, (long)message_qld, sizeof(message_qld) - 1);
    }
    syscall4(1, sum - 10, 0, 0);
}
