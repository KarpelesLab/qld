/* Compiled for Thumb: it calls A32 code, which the linker reaches with a
   `blx`, and tail-calls it through a `b.w`, which cannot change state and
   so needs an interworking thunk. */
#include <stdio.h>

extern int arm_add_qld(int n);
extern void arm_report_qld(int n);

int thumb_double_qld(int n) {
    return n * 2;
}

int thumb_tail_qld(int n) {
    return arm_add_qld(n);
}

int (*thumb_pointer_qld)(int) = arm_add_qld;

int main(void) {
    arm_report_qld(3);
    printf("thumb %d %d\n", thumb_tail_qld(4), thumb_pointer_qld(5));
    return 0;
}
