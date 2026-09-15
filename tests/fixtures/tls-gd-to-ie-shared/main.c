#include <stdio.h>
extern __thread int shared_counter;
int main(void) {
    shared_counter += 2;
    printf("tls=%d\n", shared_counter);
    return 0;
}
