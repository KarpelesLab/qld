// Globals reached through the GOT (the objects are built with -fPIC, so
// every access to a default-visibility global goes through it) and through
// ADRP+ADD pairs, near to and far from the code: the linker relaxes the
// GOT loads to ADRP+ADD and those, and the direct pairs within 1 MiB, to
// ADR. The relaxed and unrelaxed programs must print the same thing.
#include <stdio.h>

extern int near_qld;
extern int table_qld[4];
extern char far_qld[];
extern int (*callback_qld)(int);
int twice_qld(int);

static int local_counter_qld = 5;

int main(void) {
    int sum = near_qld + local_counter_qld;
    for (int i = 0; i < 4; i++) {
        sum += table_qld[i];
    }
    far_qld[0] = 3;
    sum += far_qld[0] + callback_qld(4) + twice_qld(1);
    printf("sum %d %s\n", sum, callback_qld == twice_qld ? "same" : "different");
    return 0;
}
