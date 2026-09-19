/* A freestanding x32 program (with start.c and tls_gd.c) that prints
 * values reached through every TLS model, the GOT, words the static PIE
 * relocates, and an IFUNC. */
extern void put_str(const char *);
extern void put_num(long long);

extern int tls_gd(void), tls_ld(void), tls_desc(void), tls_desc_local(void);
extern int get_a(void), call_answer(void);
extern int (*answer_ptr(void))(void);
extern __thread int t_ie;
static __thread int t_le = 44;

int data_a = 7, data_b = 8, data_c = 9;
int *ptrs[] = {&data_a, &data_b, &data_c};

int answer(void) { return 42; }
static int inc(int x) { return x + 1; }
static int plus2(int x) { return x + 2; }
int (*ops[])(int) = {inc, plus2};

static int impl_a(void) { return 99; }
__attribute__((noinline, used)) static void *pick_impl(void) { return (void *)impl_a; }
int ifunc_value(void) __attribute__((ifunc("pick_impl")));

unsigned long long big = 1234567890123ULL;
long long negative = -5;

static void sep(void) { put_str(" "); }

__attribute__((noinline)) void tls_values(void) {
    put_str("tls: ");
    put_num(tls_gd());
    sep();
    put_num(tls_ld());
    sep();
    put_num(t_ie);
    sep();
    put_num(t_le);
    sep();
    put_num(tls_desc());
    sep();
    put_num(tls_desc_local());
    put_str("\n");
}

int main(void) {
    tls_values();
    put_str("data: ");
    put_num(get_a());
    sep();
    put_num(*ptrs[1]);
    sep();
    put_num(*ptrs[2]);
    sep();
    put_num(sizeof ptrs / sizeof ptrs[0]);
    put_str("\ncalls: ");
    put_num(ops[0](11));
    sep();
    put_num(ops[1](11));
    sep();
    put_num(answer_ptr() == answer ? call_answer() : -1);
    put_str("\nifunc: ");
    put_num(ifunc_value());
    put_str("\nwide: ");
    put_num((long long)big);
    sep();
    put_num(negative);
    put_str("\n");
    return 0;
}
