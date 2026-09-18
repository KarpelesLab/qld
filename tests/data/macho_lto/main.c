/* Bitcode: the program. */
int printf(const char *, ...);
int helper(int);
int lib_bc(int);
int lib_native(int);
int from_native_object(void);
int shared_weak(void);

int main(void) {
    int value = helper(2) + lib_bc(1) + lib_native(1) + from_native_object() + shared_weak();
    printf("lto %d\n", value);
    return 0;
}
