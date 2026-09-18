/* One variable per TLS model, all in the executable, so the linker relaxes
   general-dynamic, local-dynamic, initial-exec and TLS descriptors to
   local-exec. */
#include <stdio.h>

__thread int gd_var_qld __attribute__((tls_model("global-dynamic"))) = 1;
__thread int ld_var_qld __attribute__((tls_model("local-dynamic"))) = 2;
__thread int ie_var_qld __attribute__((tls_model("initial-exec"))) = 4;
__thread int le_var_qld __attribute__((tls_model("local-exec"))) = 8;

int sum_tls_qld(void) {
    return gd_var_qld + ld_var_qld + ie_var_qld + le_var_qld;
}

int main(void) {
    gd_var_qld += 16;
    printf("tls %d\n", sum_tls_qld());
    return 0;
}
