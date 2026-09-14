#include <stdio.h>

extern void optional_hook(void) __attribute__((weak));
extern int weak_data __attribute__((weak));
const char *only_weak(void);
const char *overridden(void);

int main(void) {
    printf("optional_hook: %s\n", optional_hook ? "present" : "absent");
    printf("weak_data: %s\n", &weak_data ? "present" : "absent");
    printf("only_weak: %s\n", only_weak());
    printf("overridden: %s\n", overridden());
    return 0;
}
