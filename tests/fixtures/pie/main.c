#include <stdio.h>

struct entry {
    const char *name;
    int (*fn)(int);
    int arg;
};

extern const struct entry table[];
int main(void);

/* An absolute pointer in writable data: needs a RELATIVE relocation. */
int (*main_pointer)(void) = main;

int main(void) {
    for (const struct entry *e = table; e->name; e++)
        printf("%s: %d\n", e->name, e->fn(e->arg));
    printf("pointer to main is non-null: %s\n", main_pointer ? "yes" : "no");
    return 0;
}
