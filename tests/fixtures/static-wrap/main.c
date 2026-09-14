#include <stdio.h>
#include <stdlib.h>

int compute(int x);
int compute_directly(int x);
extern int malloc_calls;

int main(void) {
    char *a = malloc(16);
    char *b = malloc(32);
    int result = compute(4);
    printf("compute(4) = %d\n", result);
    printf("direct from defining object = %d\n", compute_directly(4));
    /* In a static link libc's own calls to malloc are wrapped too. */
    printf("wrapped malloc calls: %s\n", malloc_calls >= 2 ? "at least 2" : "too few");
    free(a);
    free(b);
    return 0;
}
