/* Undefined weak symbols, as crtbegin.o references `_ITM_*`: in data, in
 * code, through the GOT and through the PLT. In an executable they are
 * zero, and GNU ld's x86 backends emit no dynamic relocation for the
 * absolute references; a shared object keeps them. */
extern int weak_data __attribute__((weak));
extern int weak_code __attribute__((weak));
extern int weak_got __attribute__((weak));
extern void weak_fn(void) __attribute__((weak));

int *data_ptr = &weak_data;

int in_code(void) { return &weak_code == 0 ? 7 : 9; }

int through_got(void) { return weak_got; }

void call_weak(void) {
    if (weak_fn)
        weak_fn();
}

void _start(void) {
    /* exit(0), so that a linked executable is complete. */
    __asm__ volatile("syscall" ::"a"(0x4000003c), "D"(0));
}
