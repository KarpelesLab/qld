#include <stdio.h>

const char *provider(void);
const char *other(void);

/* Also defined in unused.o, which must never be extracted. */
int conflict = 1;

int main(void) {
    printf("provider: %s\n", provider());
    printf("other: %s\n", other());
    return conflict - 1;
}
