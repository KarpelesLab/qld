__attribute__((weak)) const char *only_weak(void) {
    return "weak default";
}

__attribute__((weak)) const char *overridden(void) {
    return "weak";
}
