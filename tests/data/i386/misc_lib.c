int lib_data[4] = {1, 2, 3, 4};
const char *lib_name = "the library";

static int add_a(int a, int b) { return a + b; }
static int (*resolve_add(void))(int, int) { return add_a; }
/* An IFUNC exported from the shared library. */
int lib_add(int, int) __attribute__((ifunc("resolve_add")));

int (*lib_add_ptr(void))(int, int) { return lib_add; }

int lib_sum(void) { return lib_data[0] + lib_data[1] + lib_data[2] + lib_data[3]; }
