/* A freestanding x32 shared library: TLS in the dynamic models, data an
 * executable copies, functions it calls through the PLT, an IFUNC, and
 * output through the executable's runtime (start.c). */
extern void put_str(const char *);
extern void put_num(long long);

__thread int lib_tls = 3;
static __thread int lib_tls_local = 7;
__thread int lib_tls_ie = 5;
__thread int lib_tls_gd = 6;
int lib_data[4] = {1, 2, 3, 4};
int lib_counter;

__attribute__((noinline)) int lib_get(void) { return lib_tls; }
__attribute__((noinline)) int lib_ld(void) { return lib_tls_local; }
int lib_answer(void) { return 42; }
int (*lib_answer_ptr(void))(void) { return lib_answer; }

static int times5(int x) { return 5 * x; }
__attribute__((noinline)) int lib_call(int (*f)(int), int x) { return f(x) + lib_counter; }

static int impl(void) { return 10; }
static void *pick(void) { return (void *)impl; }
int lib_pick(void) __attribute__((ifunc("pick")));

void lib_report(void) {
    put_str("lib: ");
    put_num(lib_get());
    put_str(" ");
    put_num(lib_ld());
    put_str(" ");
    put_num(lib_call(times5, 9));
    put_str(" ");
    put_num(lib_pick());
    put_str("\n");
}
