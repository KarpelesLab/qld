// A non-PIC executable that takes the address of a library function in
// code: the address is the function's PLT entry (a canonical PLT entry), so
// the indirect call lands on the entry, which under BTI must start with
// `bti c`.
#include <stdio.h>

extern int imported_qld(int);

int (*volatile pointer_qld)(int);

int main(void) {
    pointer_qld = imported_qld;
    int direct = imported_qld(1);
    int indirect = pointer_qld(2);
    printf("direct %d indirect %d\n", direct, indirect);
    return 0;
}
