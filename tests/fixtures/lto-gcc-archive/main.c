#include <stdio.h>

int archive_used_lto_qld(int);

int main(void)
{
    printf("used=%d\n", archive_used_lto_qld(4));
    return 0;
}
