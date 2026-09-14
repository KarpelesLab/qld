#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "items.h"

ITEM(alpha, 1);
ITEM(beta, 200);

extern const struct item __start_qld_items[];
extern const struct item __stop_qld_items[];

static int by_name(const void *a, const void *b) {
    return strcmp(((const struct item *)a)->name, ((const struct item *)b)->name);
}

int main(void) {
    size_t count = (size_t)(__stop_qld_items - __start_qld_items);
    struct item sorted[16];
    int sum = 0;
    if (count > 16)
        return 1;
    for (size_t i = 0; i < count; i++) {
        sorted[i] = __start_qld_items[i];
        sum += sorted[i].value;
    }
    /* Link order within the section is not what this fixture tests. */
    qsort(sorted, count, sizeof sorted[0], by_name);
    printf("count=%zu sum=%d\n", count, sum);
    for (size_t i = 0; i < count; i++)
        printf("%s=%d\n", sorted[i].name, sorted[i].value);
    return 0;
}
