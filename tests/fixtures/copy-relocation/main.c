#include <stdio.h>

extern int lib_counter;
extern char lib_message[32];
void lib_increment(void);
int lib_read(void);

int main(void) {
    printf("lib_counter=%d lib_message=%s\n", lib_counter, lib_message);
    lib_increment();
    printf("after lib_increment: %d\n", lib_counter);
    lib_counter = 500;
    printf("after exe write: lib sees %d\n", lib_read());
    return 0;
}
