// Stores of immediates to statics: on x86_64 these are `movb $imm,
// _counter-1(%rip)`-style X86_64_RELOC_SIGNED_1/_4 relocations against a
// local symbol at the start of its atom (`pcrel_immediate_stores`).
int printf(const char *, ...);

static volatile char flag;
static volatile int counter;
static volatile long long wide;

__attribute__((noinline)) void set(void) {
    flag = 1;
    counter = 0x12345678;
    wide = 0x1122334455667788LL;
}

int main(void) {
    set();
    printf("%d %x %llx\n", flag, counter, wide);
    return 0;
}
