#include <stdio.h>

int live_data_qld = 42;
int dead_data_qld = 99;

__attribute__((noinline)) int live_function_qld(void) {
    return live_data_qld;
}

__attribute__((noinline)) int dead_function_qld(void) {
    return dead_data_qld;
}

int main(void) {
    printf("%d\n", live_function_qld());
    return 0;
}
