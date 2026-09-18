// Uses init_lib.c's dylib: its `-init` routine has run before main, and
// its `-alias` name resolves to the same function.
int printf(const char *, ...);
int lib_ready(void);
int lib_ready_alias(void);

int main(void) {
    printf("ready %d %d\n", lib_ready(), lib_ready_alias());
    return 0;
}
