#include <stdio.h>

__attribute__((constructor(200))) static void ctor_200(void) {
    puts("ctor 200 (a)");
}

__attribute__((constructor)) static void ctor_plain(void) {
    puts("ctor default (a)");
}

static void ctor_raw(void) {
    puts("ctor 150 (raw section, a)");
}

__attribute__((used, section(".init_array.00150"))) static void (*const raw_entry)(void) = ctor_raw;

__attribute__((destructor(101))) static void dtor_101(void) {
    puts("dtor 101 (a)");
}

__attribute__((destructor)) static void dtor_plain(void) {
    puts("dtor default (a)");
}
