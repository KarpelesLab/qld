#include <stdio.h>

static int impl_generic(void) {
    return 1;
}

static int impl_fast(void) {
    return 2;
}

/* Runs before libc is initialized in a static binary: no library calls. */
static int (*resolve_pick(void))(void) {
    return impl_fast ? impl_fast : impl_generic;
}

int pick(void) __attribute__((ifunc("resolve_pick")));

int call_from_other(void);
int (*address_from_other(void))(void);

int main(void) {
    int (*pointer)(void) = pick;
    printf("direct: %d\n", pick());
    printf("via pointer: %d\n", pointer());
    printf("from other object: %d\n", call_from_other());
    printf("same address: %s\n", address_from_other() == pointer ? "yes" : "no");
    return 0;
}
