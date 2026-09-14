#include <stdio.h>

int tls_get(void);

int main(void) {
    printf("%d\n", tls_get());
    return 0;
}
