#include <stdio.h>

int greet_calls;

/* Defined by the executable, found through its dynamic symbol table. */
void exe_callback(const char *message, int count);

const char *greeting(void) {
    return "hello from libgreet";
}

void greet(void) {
    greet_calls++;
    exe_callback("called from the library", greet_calls);
}
