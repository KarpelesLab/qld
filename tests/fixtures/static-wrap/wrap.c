#include <stddef.h>
#include <stdio.h>

int __real_compute(int x);
void *__real_malloc(size_t size);

int malloc_calls;

int __wrap_compute(int x) {
    printf("wrapped compute(%d)\n", x);
    return __real_compute(x) + 1;
}

void *__wrap_malloc(size_t size) {
    malloc_calls++;
    return __real_malloc(size);
}
