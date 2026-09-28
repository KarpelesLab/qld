/* An IFUNC called directly. The ELFv1 canonical address of a function is
   its descriptor, and qld's IFUNC stub is code, so this program does not
   take the IFUNC's address the way `ppc64le-ifunc` does; a link that does
   is refused rather than written wrong. */
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

int main(void) {
    printf("pick %d %d\n", pick_qld(), pick_qld() + 1);
    return 0;
}
