#include <stdio.h>

__attribute__((constructor)) static void ctor_plain(void) {
    puts("ctor default (b)");
}

__attribute__((constructor(101))) static void ctor_101(void) {
    puts("ctor 101 (b)");
}

__attribute__((destructor(300))) static void dtor_300(void) {
    puts("dtor 300 (b)");
}

__attribute__((destructor)) static void dtor_plain(void) {
    puts("dtor default (b)");
}
