#include <stdio.h>

int foo(void);

#ifdef OLD_APP
int main(void) {
    printf("foo=%d\n", foo());
    return 0;
}
#else
int bar(void);

int main(void) {
    printf("foo=%d bar=%d\n", foo(), bar());
    return 0;
}
#endif
