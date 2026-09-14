#include <stdio.h>

__attribute__((noinline)) int live_function_lto_qld(int x) { return x + 1; }

int live_data_lto_qld = 41;

int native_function_lto_qld(int);

int main(void)
{
    printf("%d\n", live_function_lto_qld(live_data_lto_qld));
    printf("%d\n", native_function_lto_qld(1));
    return 0;
}
