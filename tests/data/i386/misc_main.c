#include <stdio.h>
#include <string.h>

/* IFUNC defined in the executable. */
static int impl_a(int x) { return x + 1; }
static int impl_b(int x) { return x + 2; }
int getenv_like(void);
static int (*resolve_pick(void))(int) { return getenv_like() ? impl_a : impl_b; }
int pick(int) __attribute__((ifunc("resolve_pick")));

/* Data and functions from the shared library: copy relocation (non-PIE),
   function pointer equality, lazy PLT calls. */
extern int lib_data[4];
extern const char *lib_name;
int lib_add(int, int);
int (*lib_add_ptr(void))(int, int);

int getenv_like(void) { return 1; }

int main(void) {
    int (*fp)(int, int) = lib_add;
    printf("pick=%d\n", pick(40));
    printf("lib_data=%d %d %d %d\n", lib_data[0], lib_data[1], lib_data[2], lib_data[3]);
    lib_data[2] = 42;
    printf("lib_add=%d same_ptr=%d name=%s\n", lib_add(lib_data[2], 1), fp == lib_add_ptr(),
           lib_name);
    printf("strlen=%zu\n", strlen(lib_name));
    return 0;
}
