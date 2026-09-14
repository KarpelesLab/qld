#include <stdio.h>

extern char magic_marker[];
extern long second_entry;
int alias_function(void);

long table[] = {11, 22, 33};

__attribute__((noinline)) int real_function(void) {
    return 7;
}

int main(void) {
    printf("magic_marker=%#lx\n", (unsigned long)magic_marker);
    printf("alias_function()=%d\n", alias_function());
    printf("second_entry=%ld\n", second_entry);
    return 0;
}
