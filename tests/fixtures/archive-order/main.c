#include <stdio.h>

const char *foo(void);

int main(void) {
    printf("foo calls bar: %s\n", foo());
    return 0;
}
