// Interface of the C suite's dylib (c_lib.c).
#include <stddef.h>

extern int suite_c_counter;
extern const char *suite_c_name;
extern _Thread_local int suite_c_tls;
extern int suite_c_initialized;

int suite_c_triple(int x);
int suite_c_apply(int (*callback)(int), int x);
int suite_c_weak(void);
int suite_c_call_weak(void);
int suite_c_tls_bump(void);
size_t suite_c_format(char *buffer, size_t size, double value);

// Defined by the executable, called by the plugin bundle.
int suite_c_host_value(void);
