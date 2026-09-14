int foo_v1(void) {
    return 1;
}

int foo_v2(void) {
    return 2;
}

int bar(void) {
    return 3;
}

__asm__(".symver foo_v1, foo@VERS_1");
__asm__(".symver foo_v2, foo@@VERS_2");
