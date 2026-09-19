/* The executable half of the shared library test: TLS of the library's
 * (initial-exec, or with -DPIC_MAIN general-dynamic relaxed to it), its
 * data (copied into a position-dependent executable), and its functions,
 * whose addresses must be the same seen from both sides. */
extern void put_str(const char *);
extern void put_num(long long);

extern __thread int lib_tls_ie, lib_tls_gd;
extern int lib_data[4];
extern int lib_answer(void);
extern int (*lib_answer_ptr(void))(void);
extern int lib_pick(void);
extern void lib_report(void);

static void sep(void) { put_str(" "); }

int main(void) {
    lib_report();
    put_str("main: ");
    put_num(lib_tls_ie);
    sep();
    put_num(lib_tls_gd);
    sep();
    put_num(lib_data[0]);
    sep();
    put_num(lib_data[1]);
    sep();
    put_num(lib_answer());
    sep();
    put_num(lib_answer_ptr() == lib_answer);
    sep();
    put_num(lib_pick());
    put_str("\n");
    return 0;
}
