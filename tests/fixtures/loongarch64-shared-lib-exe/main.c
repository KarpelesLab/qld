#include <stdio.h>

extern int greet_calls_qld;
extern __thread int lib_tls_qld;
void greet_qld(void);

int exe_callback_qld(int n) {
    return n * 10;
}

int main(void) {
    greet_qld();
    printf("greet_calls=%d lib_tls=%d\n", greet_calls_qld, lib_tls_qld);
    return 0;
}
