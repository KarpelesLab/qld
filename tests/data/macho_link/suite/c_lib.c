// The C suite's dylib (libsuite_c.dylib): exported functions and data, a
// weak definition the executable also defines, a thread-local variable, a
// constructor, and a callback into the executable.
#include "c_lib.h"

#include <stdio.h>
#include <string.h>

int suite_c_counter = 7;
const char *suite_c_name = "libsuite_c";
_Thread_local int suite_c_tls = 11;
int suite_c_initialized;

static int hidden_helper(int x) { return x * 3; }

__attribute__((constructor)) static void suite_c_init(void) { suite_c_initialized = 1; }

int suite_c_triple(int x) { return hidden_helper(x); }

int suite_c_apply(int (*callback)(int), int x) { return callback(x) + 1; }

__attribute__((weak)) int suite_c_weak(void) { return 1; }

int suite_c_call_weak(void) { return suite_c_weak(); }

int suite_c_tls_bump(void) { return ++suite_c_tls; }

size_t suite_c_format(char *buffer, size_t size, double value) {
    snprintf(buffer, size, "%.3f", value);
    return strlen(buffer);
}
