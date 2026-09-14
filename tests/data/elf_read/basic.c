/* Fixture for tests/elf_read.rs: a bit of everything an object can hold.
 * Deliberately free of #include so it also builds for cross targets. */

int common_var;                       /* COMMON with -fcommon */
__thread int tls_var = 5;             /* .tdata */
static __thread int tls_bss;          /* .tbss */
int data_var = 42;
static int static_var = 1;
const char *str = "hello, world";
extern int undefined_var;
extern void undefined_fn(void);

__attribute__((weak)) int weak_fn(void) { return 1; }
__attribute__((visibility("hidden"))) int hidden_fn(void) { return 2; }
__attribute__((visibility("protected"))) int protected_fn(void) { return 3; }

static int local_fn(int x) { return x * 2 + static_var; }

int use_all(void)
{
    undefined_fn();
    tls_bss++;
    return local_fn(data_var) + undefined_var + tls_var + tls_bss + common_var
        + weak_fn() + hidden_fn() + protected_fn() + str[0];
}
