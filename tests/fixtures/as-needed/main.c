#include <stdio.h>

int used_function(void);

int main(void) {
    printf("used: %d\n", used_function());
    return 0;
}
