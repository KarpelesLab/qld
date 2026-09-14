#include <stdio.h>

int lib_api_lto_qld(int);
int lib_calls_lto_qld(void);

void exe_callback_lto_qld(int n) { printf("callback %d\n", n); }

int exe_unused_lto_qld(void) { return 7; }

int main(void)
{
    printf("api=%d\n", lib_api_lto_qld(5));
    printf("calls=%d\n", lib_calls_lto_qld());
    return 0;
}
