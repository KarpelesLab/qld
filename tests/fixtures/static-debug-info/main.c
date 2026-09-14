#include <stdio.h>

static int unused_debug_function_qld(int x) __attribute__((used, noinline));
static int unused_debug_function_qld(int x) {
    return x * 3;
}

__attribute__((noinline)) int used_debug_function_qld(int x) {
    return x + 1;
}

int main(void) {
    printf("debug: %d\n", used_debug_function_qld(41));
    return 0;
}
