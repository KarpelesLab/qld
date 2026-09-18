/* Calls, tail calls, a jump table, an aligned function and local-exec
   TLS: the sequences RISC-V linker relaxation shortens. The program must
   print the same with and without relaxation. */
#include <stdio.h>

static __thread unsigned tls_counter_qld;

__attribute__((noinline)) static unsigned step_qld(unsigned x) {
    tls_counter_qld++;
    return x * 3 + 1;
}

__attribute__((noinline, aligned(64))) static unsigned aligned_qld(unsigned x) {
    return step_qld(x) ^ 5;
}

__attribute__((noinline)) unsigned dispatch_qld(unsigned op, unsigned x) {
    switch (op) {
    case 0: return step_qld(x);
    case 1: return aligned_qld(x);
    case 2: return x - 7;
    case 3: return x << 2;
    case 4: return step_qld(aligned_qld(x));
    case 5: return ~x;
    default: return 0;
    }
}

int main(void) {
    unsigned acc = 0;
    for (unsigned i = 0; i < 60; i++) {
        acc = acc * 7 + dispatch_qld(i % 7, i);
    }
    printf("relaxed %u %u\n", acc, tls_counter_qld);
    return 0;
}
