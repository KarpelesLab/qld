#include <stdio.h>

/* A TLS variable of the library, which the executable reads: the access
   stays dynamic in the library and becomes initial-exec in the
   executable. */
__thread int lib_tls_qld = 5;
int greet_calls_qld;

extern int exe_callback_qld(int n);

void greet_qld(void) {
    greet_calls_qld++;
    printf("hello from libgreet (tls=%d)\n", lib_tls_qld);
    printf("callback: %d\n", exe_callback_qld(greet_calls_qld));
}
