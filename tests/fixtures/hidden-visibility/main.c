#include <stdio.h>

int exported_qld(void);
int via_hidden_qld(void);
int protected_qld(void);

int main(void) {
    printf("exported=%d via_hidden=%d protected=%d\n", exported_qld(), via_hidden_qld(),
           protected_qld());
    return 0;
}
