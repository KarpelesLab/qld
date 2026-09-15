// A dylib: exported functions and data, a weak definition, a private
// extern, a thread-local variable and an initializer.
int printf(const char *, ...);

int greet_count = 0;
_Thread_local int greet_tls = 7;
static int initialized;

__attribute__((visibility("hidden"))) int greet_hidden(int x) { return x * 2; }

__attribute__((weak)) int greet_weak(void) { return 1; }

__attribute__((constructor)) static void greet_init(void) { initialized = 1; }

int greet(const char *name) {
    greet_count++;
    greet_tls++;
    printf("hello, %s (%d, init %d, tls %d)\n", name, greet_hidden(greet_count),
           initialized, greet_tls);
    return greet_weak();
}
