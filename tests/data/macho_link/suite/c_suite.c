// The C part of the `-fuse-ld` suite (tests/macho_link/suite.rs): a
// program built with Apple clang and qld against the real SDK, checking
// what C programs rely on from the linker. Usage: c_suite <plugin.bundle>.
#include "c_lib.h"

#include <dlfcn.h>
#include <errno.h>
#include <math.h>
#include <pthread.h>
#include <setjmp.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int checks;
static int failures;

#define CHECK(cond)                                                                \
    do {                                                                           \
        checks++;                                                                  \
        if (!(cond)) {                                                             \
            failures++;                                                            \
            printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);                 \
        }                                                                          \
    } while (0)

// Data with pointers: rebases in __DATA and __DATA_CONST.
static const char *const words[] = {"delta", "alpha", "charlie", "bravo"};
static int values[] = {5, 3, 9, 1, 7};
int *const value_ptr = &values[2];
static char big_bss[1 << 20];
static double table[256];

// Constructors run in priority order.
static int init_order[3];
static int init_count;
__attribute__((constructor(101))) static void first_init(void) { init_order[init_count++] = 1; }
__attribute__((constructor(102))) static void second_init(void) { init_order[init_count++] = 2; }
__attribute__((constructor)) static void third_init(void) { init_order[init_count++] = 3; }

// The executable's own weak definition wins over the dylib's inside the
// executable; the dylib calls its own through weak lookup.
__attribute__((weak)) int suite_c_weak(void) { return 2; }

// A weak import that no library defines (linked with `-U`).
extern int suite_c_missing(void) __attribute__((weak_import));

// `used` keeps it through `-dead_strip`: only the bundle refers to it.
__attribute__((used)) int suite_c_host_value(void) { return 100; }

static int compare_ints(const void *a, const void *b) {
    return *(const int *)a - *(const int *)b;
}

static int compare_strings(const void *a, const void *b) {
    return strcmp(*(const char *const *)a, *(const char *const *)b);
}

static int add_one(int x) { return x + 1; }

static int sum(int count, ...) {
    va_list args;
    va_start(args, count);
    int total = 0;
    for (int i = 0; i < count; i++)
        total += va_arg(args, int);
    va_end(args);
    return total;
}

static jmp_buf jump;
static __attribute__((noinline)) void deep(int n) {
    if (n == 0)
        longjmp(jump, 42);
    deep(n - 1);
}

_Thread_local int thread_counter = 5;
static void *thread_main(void *arg) {
    int id = (int)(intptr_t)arg;
    for (int i = 0; i < 1000; i++)
        thread_counter++;
    suite_c_tls_bump();
    return (void *)(intptr_t)(thread_counter + id + suite_c_tls);
}

static int atexit_ran;
static void at_exit(void) {
    if (!atexit_ran)
        printf("c suite: atexit ran\n");
    atexit_ran = 1;
}

int main(int argc, char **argv) {
    // Initializers.
    CHECK(init_count == 3);
    CHECK(init_order[0] == 1 && init_order[1] == 2 && init_order[2] == 3);
    CHECK(suite_c_initialized == 1);

    // Data and relocations.
    CHECK(*value_ptr == 9);
    CHECK(strcmp(words[1], "alpha") == 0);
    CHECK(big_bss[12345] == 0);
    big_bss[sizeof big_bss - 1] = 1;
    CHECK(big_bss[sizeof big_bss - 1] == 1);
    for (int i = 0; i < 256; i++)
        table[i] = sqrt((double)i);
    CHECK(table[144] == 12.0);
    CHECK(fabs(sin(M_PI / 2) - 1.0) < 1e-12);
    CHECK(fabs(pow(2.0, 10.0) - 1024.0) < 1e-12);

    // libc through stubs, callbacks through function pointers.
    qsort(values, 5, sizeof values[0], compare_ints);
    CHECK(values[0] == 1 && values[4] == 9);
    const char *sorted[4];
    memcpy(sorted, words, sizeof sorted);
    qsort(sorted, 4, sizeof sorted[0], compare_strings);
    CHECK(strcmp(sorted[0], "alpha") == 0 && strcmp(sorted[3], "delta") == 0);
    CHECK(sum(4, 1, 2, 3, 4) == 10);
    char buffer[64];
    snprintf(buffer, sizeof buffer, "%s-%d-%.2f", "x", 42, 1.5);
    CHECK(strcmp(buffer, "x-42-1.50") == 0);
    CHECK(strtol("0x2a", NULL, 16) == 42);
    CHECK(strtod("2.5e3", NULL) == 2500.0);
    errno = 0;
    CHECK(fopen("/nonexistent/qld-suite", "r") == NULL && errno == ENOENT);

    // setjmp and longjmp through several frames.
    int jumped = setjmp(jump);
    if (jumped == 0)
        deep(10);
    CHECK(jumped == 42);

    // The dylib: functions, data, thread-local variables, callbacks.
    CHECK(suite_c_triple(4) == 12);
    CHECK(suite_c_apply(add_one, 1) == 3);
    CHECK(suite_c_counter == 7);
    suite_c_counter++;
    CHECK(suite_c_counter == 8);
    CHECK(strcmp(suite_c_name, "libsuite_c") == 0);
    CHECK(suite_c_tls == 11);
    CHECK(suite_c_tls_bump() == 12 && suite_c_tls == 12);
    CHECK(suite_c_format(buffer, sizeof buffer, 3.14159) == 5);
    CHECK(strcmp(buffer, "3.142") == 0);

    // Weak definitions and weak imports.
    CHECK(suite_c_weak() == 2);
    int from_dylib = suite_c_call_weak();
    CHECK(from_dylib == 1 || from_dylib == 2);
    CHECK(&suite_c_missing == NULL);

    // Threads with thread-local variables.
    pthread_t threads[4];
    for (int i = 0; i < 4; i++)
        pthread_create(&threads[i], NULL, thread_main, (void *)(intptr_t)i);
    for (int i = 0; i < 4; i++) {
        void *result;
        pthread_join(threads[i], &result);
        // 5 + 1000 + i + (11 + 1): every thread starts from the initial
        // values.
        CHECK((intptr_t)result == 1005 + i + 12);
    }
    CHECK(thread_counter == 5);

    // A bundle calling back into this executable.
    if (argc > 1) {
        void *plugin = dlopen(argv[1], RTLD_NOW);
        CHECK(plugin != NULL);
        if (plugin) {
            int (*entry)(int) = (int (*)(int))dlsym(plugin, "suite_plugin_entry");
            CHECK(entry != NULL);
            if (entry)
                CHECK(entry(2) == 106);
        }
    } else {
        printf("c suite: no plugin given\n");
        failures++;
    }
    CHECK(dlsym(RTLD_DEFAULT, "suite_c_triple") != NULL);

    atexit(at_exit);
    printf("c suite: %d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
