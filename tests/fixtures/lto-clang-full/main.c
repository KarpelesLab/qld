#include <stdio.h>

int square_lto_qld(int x);
int native_add_lto_qld(int a, int b);
int native_calls_ir_lto_qld(int x);

/* Referenced from native.o only: must survive LTO. */
int ir_callee_lto_qld(int x) { return x + 1000; }

int main(void)
{
    printf("square=%d\n", square_lto_qld(7));
    printf("native=%d\n", native_add_lto_qld(40, 2));
    printf("native->ir=%d\n", native_calls_ir_lto_qld(5));
    return 0;
}
