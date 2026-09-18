/* General-dynamic and local-dynamic accesses from a shared object. */
__thread int lib_counter = 11;
static __thread int lib_private = 13;
static __thread int lib_private2 = 17;

int lib_get(void) { return lib_counter + lib_private; }

int lib_bump_ld(void) {
    lib_private += 1;
    lib_private2 += 2;
    return lib_private + lib_private2;
}
