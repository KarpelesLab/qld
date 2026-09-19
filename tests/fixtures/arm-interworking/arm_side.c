/* Compiled for A32: it calls Thumb code and takes the address of a Thumb
   function, whose symbol carries the Thumb bit that the linker must keep
   in `R_ARM_ABS32` and in the `blx` it writes for the call. */
#include <stdio.h>

extern int thumb_double_qld(int n);
extern int thumb_tail_qld(int n);

int arm_add_qld(int n) {
    return thumb_double_qld(n) + 1;
}

int (*arm_pointer_qld)(int) = thumb_tail_qld;

void arm_report_qld(int n) {
    printf("arm %d %d\n", arm_add_qld(n), arm_pointer_qld(n));
}
