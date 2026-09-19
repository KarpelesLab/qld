#include <stdio.h>
int counter = 3;
static int twice(int x) { return 2 * x; }
int main(void) {
    printf("hello %d %s\n", twice(counter), sizeof(void *) == 4 ? "x32" : "?");
    return 0;
}
