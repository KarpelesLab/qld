#include <stdio.h>

/* Linker-defined, per-module symbols: -rdynamic must not export them. */
extern char _GLOBAL_OFFSET_TABLE_[];
extern const char __ehdr_start[];

int exported_function(void) { return 42; }

int main(void) {
    printf("got=%d ehdr=%d exported=%d\n", _GLOBAL_OFFSET_TABLE_ != 0,
           __ehdr_start[0] == 0x7f, exported_function());
    return 0;
}
