/* One variable per TLS model, all in the executable. RISC-V has no
   general-dynamic relaxation, so that access still calls `__tls_get_addr`
   with a GOT pair holding module 1 and the variable's offset minus 0x800;
   initial-exec reads a GOT entry the linker fills; local-exec relaxes to a
   `tp`-relative access when the offset fits 12 bits. */
#include <stdio.h>

__thread int gd_var_qld __attribute__((tls_model("global-dynamic"))) = 1;
__thread int ld_var_qld __attribute__((tls_model("local-dynamic"))) = 2;
__thread int ie_var_qld __attribute__((tls_model("initial-exec"))) = 4;
__thread int le_var_qld __attribute__((tls_model("local-exec"))) = 8;
__thread int big_qld[1024] = {32};

int sum_tls_qld(void) {
    return gd_var_qld + ld_var_qld + ie_var_qld + le_var_qld + big_qld[0];
}

int main(void) {
    gd_var_qld += 16;
    big_qld[1000] = 64;
    printf("tls %d %d\n", sum_tls_qld(), big_qld[1000]);
    return 0;
}
