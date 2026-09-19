/* The dynamic TLS models and GOT accesses, compiled as PIC so that a
 * static link has to relax them: general-dynamic and local-dynamic, or
 * with -DDESC TLS descriptors. */
#ifdef DESC
__thread int t_desc = 55;
static __thread int t_desc_local = 66;

__attribute__((noinline)) int tls_desc(void) { return t_desc; }
__attribute__((noinline)) int tls_desc_local(void) { return t_desc_local; }
#else
__thread int t_gd = 11;
static __thread int t_ld = 22;
__thread int t_ie = 33;

extern int data_a;
extern int answer(void);

__attribute__((noinline)) int tls_gd(void) { return t_gd; }
__attribute__((noinline)) int tls_ld(void) { return t_ld; }
__attribute__((noinline)) int get_a(void) { return data_a; }
__attribute__((noinline)) int call_answer(void) { return answer(); }
__attribute__((noinline)) int (*answer_ptr(void))(void) { return answer; }
#endif
