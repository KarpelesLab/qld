#include <stdio.h>

static const char *names[] = {"alpha", "beta", "gamma"};
static const char **cursor = names;

int main(void) {
    int count = (int)(sizeof names / sizeof names[0]);
    printf("static-pie: %d entries, last=%s\n", count, cursor[count - 1]);
    return 0;
}
