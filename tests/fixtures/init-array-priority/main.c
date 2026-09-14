#include <stdio.h>

__attribute__((constructor)) static void plain_main(void) {
    puts("ctor default (main)");
}

int main(void) {
    puts("main");
    return 0;
}
