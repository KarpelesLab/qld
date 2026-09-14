__attribute__((visibility("hidden"))) int helper_hidden_qld(int x) {
    return x * 2 + 1;
}

int exported_qld(void) {
    return 10;
}

int via_hidden_qld(void) {
    return helper_hidden_qld(exported_qld());
}

__attribute__((visibility("protected"))) int protected_qld(void) {
    return 30;
}
