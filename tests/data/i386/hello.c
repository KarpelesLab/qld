#include <stdio.h>
int counter = 3;
static int twice(int x) { return 2 * x; }
int main(void) {
    printf("hello %d\n", twice(counter));
    return 0;
}
