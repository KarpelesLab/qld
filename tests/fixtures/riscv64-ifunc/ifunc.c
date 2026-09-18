#include <stdio.h>

static int impl_one_qld(void) {
    return 1;
}

static int impl_two_qld(void) {
    return 2;
}

static void *resolve_pick_qld(void) {
    return (void *)(sizeof(void *) == 8 ? impl_two_qld : impl_one_qld);
}

int pick_qld(void) __attribute__((ifunc("resolve_pick_qld")));

int (*pointer_qld)(void) = pick_qld;

int main(void) {
    printf("pick %d %d %s\n", pick_qld(), pointer_qld(),
           pointer_qld == pick_qld ? "same" : "different");
    return 0;
}
