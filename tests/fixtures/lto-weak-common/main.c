#include <stdio.h>

/* Weak definition in IR, overridden by a strong native definition. */
__attribute__((weak)) const char *overridden_lto_qld(void) { return "weak ir"; }
/* Weak definition in IR that nothing overrides. */
__attribute__((weak)) const char *only_weak_lto_qld(void) { return "weak ir kept"; }
/* Weak reference nothing defines. */
extern void optional_hook_lto_qld(void) __attribute__((weak));
/* Strong IR definition that beats a weak native one. */
const char *ir_strong_lto_qld(void) { return "strong ir"; }

/* Common symbols: IR (small) and native (large) tentative definitions. */
int common_counter_lto_qld;
char common_buffer_lto_qld[8];

const char *native_weak_caller_lto_qld(void);
void native_touch_commons_lto_qld(void);
const char *common_same_lto_qld(char *buffer);

int main(void)
{
    common_counter_lto_qld = 3;
    native_touch_commons_lto_qld();
    printf("overridden: %s\n", overridden_lto_qld());
    printf("only weak: %s\n", only_weak_lto_qld());
    printf("hook: %s\n", optional_hook_lto_qld ? "present" : "absent");
    printf("native sees: %s\n", native_weak_caller_lto_qld());
    printf("common counter: %d\n", common_counter_lto_qld);
    printf("common same: %s\n", common_same_lto_qld(common_buffer_lto_qld));
    return 0;
}
