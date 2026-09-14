#include <stdio.h>

__attribute__((noinline)) int dead_function_qld(int x) {
    return x * 3;
}

__attribute__((noinline)) int live_function_qld(int x) {
    return x + 1;
}

int dead_data_qld[100] = {1};
int live_data_qld = 41;

int main(void) {
    printf("%d\n", live_function_qld(live_data_qld));
    return 0;
}
