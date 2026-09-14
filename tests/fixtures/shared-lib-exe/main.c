#include <stdio.h>

extern int greet_calls;
const char *greeting(void);
void greet(void);

void exe_callback(const char *message, int count) {
    printf("callback: %s (%d)\n", message, count);
}

int main(void) {
    puts(greeting());
    greet();
    printf("greet_calls=%d\n", greet_calls);
    return 0;
}
