/* Bitcode: inlined into main, and one unused function LTO removes. */
int helper(int x) { return x * 10; }
int helper_unused(int x) { return x + 5; }

/* Referenced by a native object: LTO must keep it. */
int bc_called_from_native(int x) { return x + 7; }

/* Also defined weak by native.c: one definition survives. */
__attribute__((weak)) int shared_weak(void) { return 1; }
