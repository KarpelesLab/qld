__thread int tls_value = 40;
__thread int tls_zero;

int tls_get(void) {
    return tls_value + tls_zero + 2;
}
